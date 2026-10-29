//! The ticket set as typed documents.
//!
//! Markdown is only a rendering of a ticket: [`Ticket::to_markdown`] produces the one fixed
//! document from the type, so the artifact the pipeline writes and the string the code agent is
//! handed cannot drift. [`TicketSet`] is the handoff payload only -- it is deserialized from the
//! wire and validated by [`TicketSet::validate`] before the queue ever sees it, and it has no
//! markdown renderer because the artifacts are one file per ticket.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::specification::NONE;

/// The prefix of the document's single title line: the ticket id and its title.
const TITLE_PREFIX: &str = "# ";
/// The heading that opens the objective section.
pub const OBJECTIVE_HEADING: &str = "## Objective";
/// The heading that opens the acceptance-criteria section.
pub const ACCEPTANCE_HEADING: &str = "## Acceptance Criteria";
/// The heading that opens the context-and-boundaries section.
pub const CONTEXT_HEADING: &str = "## Context & Boundaries";
/// The heading that opens the scope-of-changes section.
pub const SCOPE_HEADING: &str = "## Scope of Changes";
/// The heading that opens the test-expectations section.
pub const TEST_HEADING: &str = "## Test Expectations";
/// The heading that opens the verification-gate section.
pub const GATE_HEADING: &str = "## Verification Gate";
/// Every section heading, in the order the document must use them.
pub const SECTION_HEADINGS: [&str; 6] = [
    OBJECTIVE_HEADING,
    ACCEPTANCE_HEADING,
    CONTEXT_HEADING,
    SCOPE_HEADING,
    TEST_HEADING,
    GATE_HEADING,
];
/// The sentence introducing the verification gate's fenced command. The gate is the only thing
/// that makes a ticket done, so the document states the requirement next to the command itself.
pub const GATE_SENTENCE: &str = "The exact terminal command below must exit with code 0:";
/// The prefix of a front-matter dependency bullet.
const BULLET_PREFIX: &str = "  - \"";
/// The smallest number of trimmed Unicode characters a ticket title may have.
pub const TITLE_MIN_CHARS: usize = 4;
/// The smallest number of trimmed Unicode characters a ticket objective may have.
pub const OBJECTIVE_MIN_CHARS: usize = 12;
/// The smallest number of trimmed Unicode characters a verification gate may have.
pub const GATE_MIN_CHARS: usize = 4;

/// One ticket: a discrete end-to-end slice the code agent can implement, test and verify inside a
/// single context window.
///
/// Every field is required by serde and none has a default, so an incomplete handoff is a parse
/// error the tickets agent fixes on its next step. The document the code agent receives is
/// derived from this object by [`Ticket::to_markdown`]; the model never authors the headings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Ticket {
    /// The numbered ticket identifier, of the form `<digits>-<slugs>`.
    pub id: String,
    /// The ids of tickets that must be resolved before this one.
    pub blocked_by: Vec<String>,
    /// The ids of tickets this one unblocks; documentation only, never scheduling.
    pub blocks: Vec<String>,
    /// An imperative action title.
    pub title: String,
    /// The outcome in prose.
    pub objective: String,
    /// The acceptance criteria this ticket makes true, copied from the specification without a
    /// change.
    pub acceptance_criteria: Vec<String>,
    /// What the ticket takes for granted about the tree.
    pub prerequisites_assumed: String,
    /// What this ticket must not touch.
    pub out_of_scope: Vec<String>,
    /// The changes the ticket makes.
    pub scope_of_changes: Vec<String>,
    /// The tests the ticket adds or updates.
    pub test_expectations: Vec<String>,
    /// The exact terminal command that must exit 0.
    pub verification_gate: String,
}

impl Ticket {
    /// Render this ticket as the one fixed markdown document the code agent receives.
    ///
    /// The front matter, the title line and the six section headings belong to this renderer and
    /// are never authored by the model. String fields are trimmed, empty bullets are dropped, and
    /// the returned string ends with exactly one `\n`. A dependency list with no entries renders
    /// as the key with an empty pair of brackets on a single line; a bullet-list section with no
    /// entries renders the specification module's `None.` marker.
    pub fn to_markdown(&self) -> String {
        let id = self.id.trim();
        let title = format!("{TITLE_PREFIX}{id}: {}", self.title.trim());
        let objective = format!("{OBJECTIVE_HEADING}\n\n{}", self.objective.trim());
        let acceptance = format!(
            "{ACCEPTANCE_HEADING}\n\n{}",
            bullets_markdown(&self.acceptance_criteria)
        );
        let context = format!(
            "{CONTEXT_HEADING}\n\n**Prerequisites assumed:** {}\n\n\
             **Out of scope for this ticket:**\n\n{}",
            self.prerequisites_assumed.trim(),
            bullets_markdown(&self.out_of_scope)
        );
        let scope = format!(
            "{SCOPE_HEADING}\n\n{}",
            bullets_markdown(&self.scope_of_changes)
        );
        let tests = format!(
            "{TEST_HEADING}\n\n{}",
            bullets_markdown(&self.test_expectations)
        );
        let gate = format!(
            "{GATE_HEADING}\n\n{GATE_SENTENCE}\n\n```bash\n{}\n```",
            self.verification_gate.trim()
        );

        let mut document = [
            self.front_matter(),
            title,
            objective,
            acceptance,
            context,
            scope,
            tests,
            gate,
        ]
        .join("\n\n");
        document.push('\n');
        document
    }

    /// The front-matter block: the id and the two dependency lists.
    fn front_matter(&self) -> String {
        let mut text = format!("---\nid: \"{}\"\n", self.id.trim());
        text.push_str(&dependency_list("blocked_by", &self.blocked_by));
        text.push_str(&dependency_list("blocks", &self.blocks));
        text.push_str("---");
        text
    }

    /// The name of the file this ticket is written to: the id with a markdown extension.
    ///
    /// No sanitiser is needed: the validator restricts ids to `[A-Za-z0-9._-]` and rejects
    /// duplicates, so distinct tickets cannot collide on one file.
    pub fn file_name(&self) -> String {
        format!("{}.md", self.id.trim())
    }
}

/// The whole set the tickets agent hands over, in the order it emitted the tickets.
///
/// This is a handoff payload, not a document: the artifacts are one file per ticket, so there is
/// no set renderer and no reverse parser. [`TicketSet::validate`] is the only gate before the
/// queue: a set the queue could not execute is refused at the handoff so the tickets agent can
/// correct its arguments on its next step.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TicketSet {
    /// The tickets, in the order the tickets agent emitted them.
    pub tickets: Vec<Ticket>,
}

impl TicketSet {
    /// Return the first validation failure as a message the tickets agent can act on.
    ///
    /// The rules are checked in one fixed order, so the reported failure is deterministic. The
    /// structural rules come first: the set is non-empty; every id is non-empty and contains only
    /// `[A-Za-z0-9._-]`; ids are unique; every `blocked_by` and `blocks` entry names an id in the
    /// set; and the blocking edges are acyclic. Then every ticket is checked in the order it was
    /// emitted, and within one ticket in field order: `title`, `objective` and
    /// `verification_gate` must each have at least [`TITLE_MIN_CHARS`], [`OBJECTIVE_MIN_CHARS`]
    /// and [`GATE_MIN_CHARS`] trimmed Unicode characters; the objective must not restate the title
    /// after trimming, Unicode case folding and collapsing consecutive whitespace;
    /// `prerequisites_assumed` must be non-blank; and at least one of `out_of_scope`,
    /// `scope_of_changes`, `test_expectations` and `acceptance_criteria` must hold a non-blank
    /// entry, because blank entries carry no content. `blocks` stays documentation (it never
    /// affects scheduling) and no other content is judged.
    pub fn validate(&self) -> Result<(), String> {
        if self.tickets.is_empty() {
            return Err(
                "[ERROR] the ticket set is empty; hand over at least one ticket that implements \
                 a slice of the specification"
                    .to_string(),
            );
        }

        for ticket in &self.tickets {
            let id = ticket.id.trim();
            if id.is_empty() {
                return Err(
                    "[ERROR] a ticket id is empty; every ticket needs a non-empty id of the form \
                     `<digits>-<slugs>`"
                        .to_string(),
                );
            }
            if let Some(bad) = id.chars().find(|c| !is_id_char(*c)) {
                return Err(format!(
                    "[ERROR] the ticket id `{id}` contains `{bad}`, which is not allowed; ids may \
                     only contain letters, digits and `.`, `_`, `-`"
                ));
            }
        }

        let mut index_of: HashMap<&str, usize> = HashMap::new();
        for (index, ticket) in self.tickets.iter().enumerate() {
            let id = ticket.id.trim();
            if index_of.insert(id, index).is_some() {
                return Err(format!(
                    "[ERROR] the ticket id `{id}` is duplicated; every ticket needs a unique id"
                ));
            }
        }

        for ticket in &self.tickets {
            for group in [&ticket.blocked_by, &ticket.blocks] {
                for entry in group {
                    let dependency = entry.trim();
                    if !dependency.is_empty() && !index_of.contains_key(dependency) {
                        return Err(format!(
                            "[ERROR] ticket `{}` names `{dependency}`, which is not a ticket in \
                             this set; every dependency id must exist",
                            ticket.id.trim()
                        ));
                    }
                }
            }
        }

        if let Some((from, to)) = detect_cycle(&self.tickets, &index_of) {
            return Err(format!(
                "[ERROR] the ticket set has a dependency cycle: `{from}` is blocked by `{to}`, \
                 which is blocked by `{from}` again; remove the cycle so the tickets can be \
                 ordered"
            ));
        }

        for ticket in &self.tickets {
            let id = ticket.id.trim();

            let title = ticket.title.trim();
            let title_chars = title.chars().count();
            if title_chars < TITLE_MIN_CHARS {
                return Err(format!(
                    "[ERROR] ticket `{id}` has a {title_chars}-character title after trimming; the \
                     `title` must be at least {TITLE_MIN_CHARS} characters and name the ticket's \
                     outcome"
                ));
            }

            let objective = ticket.objective.trim();
            let objective_chars = objective.chars().count();
            if objective_chars < OBJECTIVE_MIN_CHARS {
                return Err(format!(
                    "[ERROR] ticket `{id}` has a {objective_chars}-character objective after \
                     trimming; the `objective` must be at least {OBJECTIVE_MIN_CHARS} characters \
                     and state the outcome in prose"
                ));
            }

            if normalize_content(title) == normalize_content(objective) {
                return Err(format!(
                    "[ERROR] ticket `{id}` repeats its `title` in its `objective`; the objective \
                     must add the outcome the ticket achieves, not restate the title"
                ));
            }

            let gate = ticket.verification_gate.trim();
            if gate.is_empty() {
                return Err(format!(
                    "[ERROR] ticket `{id}` has no verification gate; the gate must be a real \
                     command that exits 0, because a ticket is resolved only once its gate does"
                ));
            }
            let gate_chars = gate.chars().count();
            if gate_chars < GATE_MIN_CHARS {
                return Err(format!(
                    "[ERROR] ticket `{id}` has a {gate_chars}-character `verification_gate` after \
                     trimming; the gate must be at least {GATE_MIN_CHARS} characters and be the \
                     exact terminal command that proves the ticket"
                ));
            }

            if ticket.prerequisites_assumed.trim().is_empty() {
                return Err(format!(
                    "[ERROR] ticket `{id}` has blank `prerequisites_assumed`; state what the \
                     ticket takes for granted about the tree, or write `None.` when it assumes \
                     nothing"
                ));
            }

            let has_content = [
                &ticket.out_of_scope,
                &ticket.scope_of_changes,
                &ticket.test_expectations,
                &ticket.acceptance_criteria,
            ]
            .iter()
            .any(|entries| entries.iter().any(|entry| !entry.trim().is_empty()));
            if !has_content {
                return Err(format!(
                    "[ERROR] ticket `{id}` has no content: `out_of_scope`, `scope_of_changes`, \
                     `test_expectations` and `acceptance_criteria` are all blank; add at least one \
                     non-blank entry so the code agent knows what to do"
                ));
            }
            let non_blank_tests: Vec<&str> = ticket
                .test_expectations
                .iter()
                .map(|entry| entry.trim())
                .filter(|entry| !entry.is_empty())
                .collect();
            if non_blank_tests.is_empty() {
                return Err(format!(
                    "[ERROR] ticket `{id}` has no `test_expectations`; every ticket must list at \
                     least one test expectation naming a \
                     `when_<action>_given_<context>_then_<outcome>` test case"
                ));
            }
            for entry in non_blank_tests {
                if !has_bdd_test_name(entry) {
                    return Err(format!(
                        "[ERROR] ticket `{id}` has a `test_expectations` entry without a \
                         `when_<action>_given_<context>_then_<outcome>` test identifier: \
                         \"{entry}\"; every test expectation must name its test in lowercase \
                         snake_case `when_<action>_given_<context>_then_<outcome>` form"
                    ));
                }
            }
        }

        Ok(())
    }
}

/// Whether `text` contains a valid `when_<action>_given_<context>_then_<outcome>`
/// identifier token in lowercase `snake_case`.
pub fn has_bdd_test_name(text: &str) -> bool {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .any(is_bdd_test_identifier)
}

/// Whether `token` is a single `when_<action>_given_<context>_then_<outcome>`
/// identifier with non-empty lowercase alphanumeric segments.
fn is_bdd_test_identifier(token: &str) -> bool {
    if !token
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        return false;
    }
    let Some(after_when) = token.strip_prefix("when_") else {
        return false;
    };
    let Some((action, after_given)) = after_when.split_once("_given_") else {
        return false;
    };
    let Some((context, outcome)) = after_given.split_once("_then_") else {
        return false;
    };
    has_segment_chars(action) && has_segment_chars(context) && has_segment_chars(outcome)
}

/// Whether `segment` is non-empty and contains at least one lowercase letter or digit.
fn has_segment_chars(segment: &str) -> bool {
    !segment.is_empty()
        && segment
            .chars()
            .any(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

/// Whether `c` is allowed in a ticket id: a letter, a digit, or `.`, `_` or `-`.
pub(crate) fn is_id_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')
}

/// Fold content for the title/objective comparison: trim, Unicode case fold, and collapse every
/// run of whitespace to a single space.
///
/// `str::to_lowercase` is the standard library's Unicode-aware case mapping; the fold plus the
/// whitespace collapse means `"Add   The Ticket"` and `"add the ticket"` compare equal.
fn normalize_content(text: &str) -> String {
    text.split_whitespace()
        .map(str::to_lowercase)
        .collect::<Vec<String>>()
        .join(" ")
}

/// Find the back edge that closes a cycle among the `blocked_by` edges, if there is one.
///
/// A depth-first search with colours over the emitted order: a white node is unvisited, a grey
/// node is on the current path and a black node is finished. An edge into a grey node is a back
/// edge -- `(from, to)` -- and is reported so the model can name the ids that close the cycle. A
/// self-edge is the degenerate case where `from == to`.
fn detect_cycle(tickets: &[Ticket], index_of: &HashMap<&str, usize>) -> Option<(String, String)> {
    fn visit(
        node: usize,
        tickets: &[Ticket],
        index_of: &HashMap<&str, usize>,
        color: &mut [u8],
    ) -> Option<(String, String)> {
        color[node] = 1;
        for entry in &tickets[node].blocked_by {
            let dependency = entry.trim();
            let Some(&next) = index_of.get(dependency) else {
                continue;
            };
            match color[next] {
                0 => {
                    if let Some(edge) = visit(next, tickets, index_of, color) {
                        return Some(edge);
                    }
                }
                1 => return Some((tickets[node].id.trim().to_string(), dependency.to_string())),
                _ => {}
            }
        }
        color[node] = 2;
        None
    }

    let mut color = vec![0u8; tickets.len()];
    for start in 0..tickets.len() {
        if color[start] == 0
            && let Some(edge) = visit(start, tickets, index_of, &mut color)
        {
            return Some(edge);
        }
    }
    None
}

/// One front-matter dependency list: `key: []` when empty, otherwise a nested list of quoted ids.
fn dependency_list(key: &str, entries: &[String]) -> String {
    let items: Vec<&str> = entries
        .iter()
        .map(|entry| entry.trim())
        .filter(|entry| !entry.is_empty())
        .collect();

    if items.is_empty() {
        return format!("{key}: []\n");
    }

    let mut text = format!("{key}:\n");
    for item in items {
        text.push_str(&format!("{BULLET_PREFIX}{item}\"\n"));
    }
    text
}

/// A bullet-list section body: one bullet per non-blank entry, or [`NONE`] when there are none.
fn bullets_markdown(entries: &[String]) -> String {
    let bullets: Vec<String> = entries
        .iter()
        .map(|entry| entry.trim())
        .filter(|entry| !entry.is_empty())
        .map(|entry| format!("- {entry}"))
        .collect();

    if bullets.is_empty() {
        NONE.to_string()
    } else {
        bullets.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fixture the whole-document assertion is built around.
    fn sample() -> Ticket {
        Ticket {
            id: "1-add-the-type".to_string(),
            blocked_by: vec!["0-bootstrap".to_string()],
            blocks: vec!["2-render".to_string()],
            title: "Add the ticket type".to_string(),
            objective: "Own the ticket contract as a typed document.".to_string(),
            acceptance_criteria: vec!["The ticket type carries its criteria.".to_string()],
            prerequisites_assumed: "The crate builds.".to_string(),
            out_of_scope: vec!["Content validation.".to_string()],
            scope_of_changes: vec!["Add `Ticket`.".to_string()],
            test_expectations: vec!["The renderer emits the template.".to_string()],
            verification_gate: "cargo test tickets".to_string(),
        }
    }

    /// The whole document the fixture renders to.
    fn expected() -> &'static str {
        r#"---
id: "1-add-the-type"
blocked_by:
  - "0-bootstrap"
blocks:
  - "2-render"
---

# 1-add-the-type: Add the ticket type

## Objective

Own the ticket contract as a typed document.

## Acceptance Criteria

- The ticket type carries its criteria.

## Context & Boundaries

**Prerequisites assumed:** The crate builds.

**Out of scope for this ticket:**

- Content validation.

## Scope of Changes

- Add `Ticket`.

## Test Expectations

- The renderer emits the template.

## Verification Gate

The exact terminal command below must exit with code 0:

```bash
cargo test tickets
```
"#
    }

    #[test]
    fn renders_the_fixed_template() {
        assert_eq!(sample().to_markdown(), expected());
    }

    #[test]
    fn the_document_opens_with_the_front_matter_and_title() {
        let doc = sample().to_markdown();
        assert!(doc.starts_with(
            "---\nid: \"1-add-the-type\"\nblocked_by:\n  - \"0-bootstrap\"\nblocks:\n  - \
             \"2-render\"\n---\n\n# 1-add-the-type: Add the ticket type\n"
        ));
    }

    /// The front matter carries the id and the two dependency lists and nothing else: the
    /// specification reference the code agent cannot use is gone.
    #[test]
    fn the_front_matter_carries_only_the_id_and_dependency_lists() {
        let doc = sample().to_markdown();
        let front_matter = doc
            .split("\n---\n")
            .next()
            .expect("the document opens with front matter");
        let body = front_matter
            .strip_prefix("---\n")
            .expect("the front matter opens with `---`");
        let keys: Vec<&str> = body
            .lines()
            .filter(|line| !line.starts_with("  - "))
            .map(|line| line.split(':').next().expect("every line has a key"))
            .collect();
        assert_eq!(keys, ["id", "blocked_by", "blocks"]);
        assert!(!doc.contains("spec_ref"), "`spec_ref` must not render");
    }

    #[test]
    fn every_section_appears_once_in_order() {
        let doc = sample().to_markdown();
        let mut previous = 0;
        for heading in SECTION_HEADINGS {
            assert_eq!(doc.matches(heading).count(), 1, "`{heading}` once");
            let position = doc.find(heading).expect("heading is present");
            assert!(position >= previous, "`{heading}` in order");
            previous = position;
        }
    }

    /// Acceptance criteria render verbatim, one bullet per entry, under one fixed heading.
    #[test]
    fn acceptance_criteria_render_verbatim_as_bullets() {
        let mut ticket = sample();
        ticket.acceptance_criteria = vec![
            "The renderer emits the criteria verbatim.".to_string(),
            "A second criterion keeps its `exact` text.".to_string(),
        ];
        let doc = ticket.to_markdown();
        assert!(doc.contains(
            "## Acceptance Criteria\n\n\
             - The renderer emits the criteria verbatim.\n\
             - A second criterion keeps its `exact` text.\n\n\
             ## Context & Boundaries"
        ));
        assert_eq!(doc.matches(ACCEPTANCE_HEADING).count(), 1);
    }

    #[test]
    fn acceptance_criteria_are_trimmed_and_blank_entries_dropped() {
        let mut ticket = sample();
        ticket.acceptance_criteria = vec![
            "  First criterion.  ".to_string(),
            "   ".to_string(),
            "Second criterion.".to_string(),
        ];
        let doc = ticket.to_markdown();
        assert!(doc.contains(
            "## Acceptance Criteria\n\n- First criterion.\n- Second criterion.\n\n\
             ## Context & Boundaries"
        ));
    }

    #[test]
    fn empty_acceptance_criteria_render_none() {
        let mut ticket = sample();
        ticket.acceptance_criteria.clear();
        let doc = ticket.to_markdown();
        assert!(doc.contains("## Acceptance Criteria\n\nNone.\n\n## Context & Boundaries"));
    }

    #[test]
    fn empty_dependency_lists_render_on_one_line() {
        let mut ticket = sample();
        ticket.blocked_by.clear();
        ticket.blocks.clear();
        let doc = ticket.to_markdown();
        assert!(doc.contains("blocked_by: []\nblocks: []\n---"));
        assert_eq!(doc.matches("blocked_by: []").count(), 1);
    }

    #[test]
    fn empty_bullet_sections_render_none() {
        let mut ticket = sample();
        ticket.acceptance_criteria.clear();
        ticket.out_of_scope.clear();
        ticket.scope_of_changes.clear();
        ticket.test_expectations.clear();
        let doc = ticket.to_markdown();
        assert!(doc.contains("## Acceptance Criteria\n\nNone.\n"));
        assert!(doc.contains(
            "**Out of scope for this ticket:**\n\nNone.\n\n## Scope of Changes\n\nNone.\n\n\
             ## Test Expectations\n\nNone.\n\n## Verification Gate"
        ));
    }

    #[test]
    fn field_values_are_trimmed() {
        let mut ticket = sample();
        ticket.id = "  1-add-the-type  ".to_string();
        ticket.title = "  Add the ticket type  ".to_string();
        ticket.objective = "\n  Own the ticket contract as a typed document.  \n".to_string();
        ticket.verification_gate = "  cargo test tickets  ".to_string();
        assert_eq!(ticket.to_markdown(), expected());
    }

    #[test]
    fn blank_bullets_are_dropped() {
        let mut ticket = sample();
        ticket.scope_of_changes = vec!["   ".to_string(), "  Add `Ticket`.  ".to_string()];
        let doc = ticket.to_markdown();
        assert!(doc.contains("## Scope of Changes\n\n- Add `Ticket`.\n\n## Test Expectations"));
    }

    #[test]
    fn the_document_ends_with_exactly_one_newline() {
        let doc = sample().to_markdown();
        assert!(doc.ends_with('\n'));
        assert!(!doc.ends_with("\n\n"));
    }

    #[test]
    fn the_file_name_is_the_id_with_a_markdown_extension() {
        assert_eq!(sample().file_name(), "1-add-the-type.md");
        let mut ticket = sample();
        ticket.id = "  2-render  ".to_string();
        assert_eq!(ticket.file_name(), "2-render.md");
    }

    /// The base for the validation tests: a set that passes every rule.
    fn valid_set() -> TicketSet {
        TicketSet {
            tickets: vec![
                Ticket {
                    id: "1-first".to_string(),
                    blocked_by: Vec::new(),
                    blocks: vec!["2-second".to_string()],
                    title: "First".to_string(),
                    objective: "Do the first thing.".to_string(),
                    acceptance_criteria: vec!["The first criterion.".to_string()],
                    prerequisites_assumed: "Nothing.".to_string(),
                    out_of_scope: Vec::new(),
                    scope_of_changes: vec!["One change.".to_string()],
                    test_expectations: vec![
                        "when_first_runs_given_setup_then_succeeds".to_string(),
                    ],
                    verification_gate: "cargo check".to_string(),
                },
                Ticket {
                    id: "2-second".to_string(),
                    blocked_by: vec!["1-first".to_string()],
                    blocks: Vec::new(),
                    title: "Second".to_string(),
                    objective: "Do the second thing.".to_string(),
                    acceptance_criteria: vec!["The second criterion.".to_string()],
                    prerequisites_assumed: "The first ticket landed.".to_string(),
                    out_of_scope: Vec::new(),
                    scope_of_changes: vec!["Another change.".to_string()],
                    test_expectations: vec![
                        "when_second_runs_given_first_then_succeeds".to_string(),
                    ],
                    verification_gate: "cargo test".to_string(),
                },
            ],
        }
    }

    /// The schema derived from the type is the contract the tickets agent fills:
    /// it requires the acceptance criteria and no longer exposes `spec_ref`.
    #[test]
    fn the_ticket_schema_requires_acceptance_criteria_and_excludes_spec_ref() {
        let schema = serde_json::to_value(schemars::schema_for!(Ticket)).unwrap();

        let required: Vec<&str> = schema["required"]
            .as_array()
            .expect("the ticket schema lists its required fields")
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect();
        assert!(
            required.contains(&"acceptance_criteria"),
            "the schema must require `acceptance_criteria`: {required:?}"
        );
        assert!(
            !required.contains(&"spec_ref"),
            "the schema must not require `spec_ref`: {required:?}"
        );

        let properties = schema["properties"]
            .as_object()
            .expect("the ticket schema lists its properties");
        assert!(properties.contains_key("acceptance_criteria"));
        assert!(!properties.contains_key("spec_ref"));
    }

    #[test]
    fn the_valid_set_passes() {
        assert_eq!(valid_set().validate(), Ok(()));
    }

    /// The first rule: a set with no tickets is refused and says it is empty.
    #[test]
    fn an_empty_set_is_refused() {
        let error = TicketSet {
            tickets: Vec::new(),
        }
        .validate()
        .expect_err("must not validate");
        assert!(error.contains("empty"), "{error}");
    }

    /// The second rule: an empty id, or a character outside the allowed set, is refused by name.
    #[test]
    fn an_invalid_id_is_refused() {
        let mut set = valid_set();
        set.tickets[0].id = String::new();
        let error = set.validate().expect_err("must not validate");
        assert!(error.contains("empty"), "{error}");

        let mut set = valid_set();
        set.tickets[1].id = "2 second".to_string();
        let error = set.validate().expect_err("must not validate");
        assert!(error.contains("2 second"), "{error}");
        assert!(error.contains("not allowed"), "{error}");
    }

    /// The third rule: a repeated id is refused and the duplicated id is named.
    #[test]
    fn a_duplicate_id_is_refused() {
        let mut set = valid_set();
        set.tickets[1].id = "1-first".to_string();
        let error = set.validate().expect_err("must not validate");
        assert!(error.contains("duplicated"), "{error}");
        assert!(error.contains("1-first"), "{error}");
    }

    /// The fourth rule: a dependency naming an id absent from the set is refused by name.
    #[test]
    fn a_missing_dependency_is_refused() {
        let mut set = valid_set();
        set.tickets[1].blocked_by = vec!["9-absent".to_string()];
        let error = set.validate().expect_err("must not validate");
        assert!(error.contains("9-absent"), "{error}");

        let mut set = valid_set();
        set.tickets[1].blocks = vec!["9-absent".to_string()];
        set.tickets[1].blocked_by.clear();
        let error = set.validate().expect_err("must not validate");
        assert!(error.contains("9-absent"), "{error}");
    }

    /// The fifth rule: a cycle is refused and the ids of the back edge that closes it are named.
    #[test]
    fn a_cycle_is_refused() {
        let mut set = valid_set();
        set.tickets[0].blocked_by = vec!["2-second".to_string()];
        let error = set.validate().expect_err("must not validate");
        assert!(error.contains("cycle"), "{error}");
        assert!(error.contains("1-first"), "{error}");
        assert!(error.contains("2-second"), "{error}");
    }

    /// A self-edge is the degenerate cycle and is refused the same way.
    #[test]
    fn a_self_edge_is_refused() {
        let mut set = valid_set();
        set.tickets[0].blocked_by = vec!["1-first".to_string()];
        let error = set.validate().expect_err("must not validate");
        assert!(error.contains("cycle"), "{error}");
        assert!(error.contains("1-first"), "{error}");
    }

    /// The sixth rule: a blank verification gate is refused and says the gate must be real.
    #[test]
    fn a_blank_gate_is_refused() {
        let mut set = valid_set();
        set.tickets[1].verification_gate = "   ".to_string();
        let error = set.validate().expect_err("must not validate");
        assert!(error.contains("verification gate"), "{error}");
        assert!(error.contains("2-second"), "{error}");
    }

    /// The first failure in the order is the one reported.
    #[test]
    fn the_first_failure_in_order_is_reported() {
        let mut set = valid_set();
        set.tickets[0].id = "1 first".to_string();
        set.tickets[1].id = "1 first".to_string();
        set.tickets[1].verification_gate = String::new();
        let error = set.validate().expect_err("must not validate");
        assert!(error.contains("not allowed"), "{error}");
    }

    /// The title minimum is a trimmed Unicode-character count: a multibyte character counts once,
    /// and the whitespace around a title does not count towards it.
    #[test]
    fn the_title_minimum_counts_trimmed_unicode_characters() {
        let mut set = valid_set();
        set.tickets[0].title = "abc".to_string();
        let error = set.validate().expect_err("must not validate");
        assert!(error.contains("1-first"), "{error}");
        assert!(error.contains("title"), "{error}");

        // Three characters but four bytes: the rule counts characters, not bytes.
        let mut set = valid_set();
        set.tickets[0].title = "abé".to_string();
        let error = set.validate().expect_err("must not validate");
        assert!(error.contains("title"), "{error}");

        // Exactly four characters, wrapped in whitespace that trimming removes.
        let mut set = valid_set();
        set.tickets[0].title = "  abéé  ".to_string();
        assert_eq!(set.validate(), Ok(()));
    }

    /// The objective minimum is a trimmed Unicode-character count too.
    #[test]
    fn the_objective_minimum_counts_trimmed_unicode_characters() {
        let mut set = valid_set();
        set.tickets[0].objective = "Make it wor".to_string(); // eleven characters
        let error = set.validate().expect_err("must not validate");
        assert!(error.contains("1-first"), "{error}");
        assert!(error.contains("objective"), "{error}");

        // Exactly twelve characters.
        let mut set = valid_set();
        set.tickets[0].objective = "  Make it work  ".to_string();
        assert_eq!(set.validate(), Ok(()));
    }

    /// The gate minimum is a trimmed Unicode-character count as well.
    #[test]
    fn the_gate_minimum_counts_trimmed_unicode_characters() {
        let mut set = valid_set();
        set.tickets[0].verification_gate = "abc".to_string();
        let error = set.validate().expect_err("must not validate");
        assert!(error.contains("1-first"), "{error}");
        assert!(error.contains("verification_gate"), "{error}");

        // Three characters but six bytes.
        let mut set = valid_set();
        set.tickets[0].verification_gate = "ééé".to_string();
        let error = set.validate().expect_err("must not validate");
        assert!(error.contains("verification_gate"), "{error}");

        // Exactly four characters.
        let mut set = valid_set();
        set.tickets[0].verification_gate = "true".to_string();
        assert_eq!(set.validate(), Ok(()));
    }

    /// An objective that is the title again after whitespace collapsing and case folding is
    /// refused; the error names the ticket and both fields.
    #[test]
    fn an_objective_that_repeats_the_title_is_refused() {
        let mut set = valid_set();
        set.tickets[0].title = "Add the parser".to_string();
        set.tickets[0].objective = "  add   the\nparser  ".to_string();
        let error = set.validate().expect_err("must not validate");
        assert!(error.contains("1-first"), "{error}");
        assert!(error.contains("objective"), "{error}");
        assert!(error.contains("title"), "{error}");
    }

    /// The equivalence rule folds Unicode case, not only ASCII case.
    #[test]
    fn the_title_objective_comparison_folds_unicode_case() {
        let mut set = valid_set();
        set.tickets[0].title = "Étape détaillée".to_string();
        set.tickets[0].objective = "ÉTAPE   DÉTAILLÉE".to_string();
        let error = set.validate().expect_err("must not validate");
        assert!(error.contains("objective"), "{error}");
    }

    /// A distinct objective of sufficient length is the control that keeps the equivalence rule
    /// from rejecting every ticket.
    #[test]
    fn a_distinct_objective_is_not_refused() {
        let mut set = valid_set();
        set.tickets[0].title = "Add the parser".to_string();
        set.tickets[0].objective = "Parse the ticket set before it reaches the queue.".to_string();
        assert_eq!(set.validate(), Ok(()));
    }

    /// Blank prerequisites are refused and the error names the field to fill.
    #[test]
    fn blank_prerequisites_are_refused() {
        let mut set = valid_set();
        set.tickets[0].prerequisites_assumed = "   \n".to_string();
        let error = set.validate().expect_err("must not validate");
        assert!(error.contains("1-first"), "{error}");
        assert!(error.contains("prerequisites_assumed"), "{error}");
    }

    /// A ticket whose four content lists are all empty has nothing to implement and is refused.
    #[test]
    fn a_ticket_with_no_content_in_any_list_is_refused() {
        let mut set = valid_set();
        set.tickets[0].out_of_scope.clear();
        set.tickets[0].scope_of_changes.clear();
        set.tickets[0].test_expectations.clear();
        set.tickets[0].acceptance_criteria.clear();
        let error = set.validate().expect_err("must not validate");
        assert!(error.contains("1-first"), "{error}");
        for list in [
            "out_of_scope",
            "scope_of_changes",
            "test_expectations",
            "acceptance_criteria",
        ] {
            assert!(error.contains(list), "`{list}` must be named: {error}");
        }
    }

    /// Whitespace-only entries are blank: they do not count as content.
    #[test]
    fn blank_entries_do_not_count_as_content() {
        let mut set = valid_set();
        set.tickets[0].out_of_scope = vec!["  ".to_string()];
        set.tickets[0].scope_of_changes = vec!["\t".to_string()];
        set.tickets[0].test_expectations = vec!["\n".to_string()];
        set.tickets[0].acceptance_criteria = vec!["   ".to_string()];
        let error = set.validate().expect_err("must not validate");
        assert!(error.contains("1-first"), "{error}");
    }

    /// One non-blank entry anywhere among the four lists is enough: the lists are alternatives,
    /// not all required.
    #[test]
    fn one_populated_list_is_enough_content() {
        let mut set = valid_set();
        set.tickets[0].out_of_scope.clear();
        set.tickets[0].scope_of_changes.clear();
        set.tickets[0].test_expectations =
            vec!["  when_running_test_given_input_then_passes  ".to_string()];
        set.tickets[0].acceptance_criteria.clear();
        assert_eq!(set.validate(), Ok(()));
    }

    /// Within one ticket the fields are checked in a fixed order: a bad title is reported before
    /// the other content failures it sits beside.
    #[test]
    fn the_first_content_failure_in_order_is_reported() {
        let mut set = valid_set();
        set.tickets[0].title = "abc".to_string();
        set.tickets[0].objective = String::new();
        set.tickets[0].verification_gate = String::new();
        let error = set.validate().expect_err("must not validate");
        assert!(error.contains("title"), "{error}");
        assert!(!error.contains("objective"), "{error}");
        assert!(!error.contains("verification gate"), "{error}");
    }

    /// Tickets are checked in the order they were emitted, so the first bad ticket is named even
    /// when a later ticket fails the same rule.
    #[test]
    fn the_first_bad_ticket_in_emitted_order_is_reported() {
        let mut set = valid_set();
        set.tickets[0].title = "abc".to_string();
        set.tickets[1].title = "xy".to_string();
        let error = set.validate().expect_err("must not validate");
        assert!(error.contains("1-first"), "{error}");
        assert!(!error.contains("2-second"), "{error}");
    }

    #[test]
    fn when_validating_ticket_given_empty_test_expectations_then_refuses() {
        let mut set = valid_set();
        set.tickets[0].test_expectations.clear();
        let error = set.validate().expect_err("must not validate");
        assert!(error.contains("1-first"), "{error}");
        assert!(error.contains("test_expectations"), "{error}");
    }

    #[test]
    fn when_validating_ticket_given_non_bdd_test_expectation_then_refuses() {
        let mut set = valid_set();
        set.tickets[0].test_expectations = vec!["it tests the first slice".to_string()];
        let error = set.validate().expect_err("must not validate");
        assert!(error.contains("1-first"), "{error}");
        assert!(
            error.contains("when_<action>_given_<context>_then_<outcome>"),
            "{error}"
        );
    }

    #[test]
    fn when_validating_ticket_given_bdd_test_identifier_in_prose_then_passes() {
        let mut set = valid_set();
        set.tickets[0].test_expectations = vec![
            "`when_parsing_set_given_valid_json_then_returns_ok` in `src/tickets.rs`.".to_string(),
        ];
        assert_eq!(set.validate(), Ok(()));
    }
}
