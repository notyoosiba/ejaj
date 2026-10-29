//! The triage report as a typed document.
//!
//! Triaging runs after the tickets handoff and before the ticket queue: for
//! each draft ticket the tickets agent emitted, the triage pass records a
//! category (bug or enhancement), one of four states (ready-for-agent,
//! ready-for-human, needs-info or wontfix), at least one audit note and --
//! when the ticket is ready for the code agent -- an [AgentBrief] restating the
//! target behavior, its machine-verifiable acceptance criteria, the boundaries
//! the agent must not cross, and the dependencies the draft ticket declared.
//!
//! Markdown is only a rendering of the type: [TriageReport::to_markdown]
//! produces the ticket-triage.md artifact from the typed report, in
//! draft-ticket order, so the document the pipeline writes and the report the
//! pass handed over cannot drift. The report is an audit beside plan.md and
//! spec.md, never an execution contract: the ticket files under tickets/ stay
//! the rendering of the original [Ticket] values, and the queue executes those.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

use crate::specification::{NONE, Specification};
use crate::tickets::{Ticket, TicketSet};

/// The name of the artifact [TriageReport::to_markdown] produces: it sits in
/// the run's artifact directory beside plan.md and spec.md.
pub const TRIAGE_ARTIFACT: &str = "ticket-triage.md";
/// The document title of the rendered artifact.
pub const TRIAGE_TITLE: &str = "# Ticket triage report";
/// The heading that opens a ready classification's Agent Brief.
pub const BRIEF_HEADING: &str = "### Agent Brief";
/// The boundary-matrix entry for user authorization.
///
/// Triaging a draft ticket set has no user-authorization boundary: a
/// classification reads the draft set and writes a report, so it authorizes
/// nothing, executes nothing, and touches no queue state. The one capability
/// boundary near triage -- the read-only tool set the pass is offered -- belongs
/// to the agent/tool ticket that registers the pass, not to this contract. The
/// dimension is recorded here so the boundary matrix has an explicit answer.
pub const AUTHORIZATION_BOUNDARY: &str = "Not applicable: triaging the draft ticket set has no \
                                         user authorization boundary, because a classification \
                                         reads the draft set and writes a report -- it authorizes \
                                         nothing and executes nothing. The read-only tool \
                                         capabilities the triage pass is offered are covered by \
                                         the agent/tool ticket that registers the pass.";

/// What kind of change a draft ticket carries.
///
/// The set is closed and has exactly two members: a bug corrects behavior that
/// already exists, including a regression, and an enhancement adds new behavior
/// or capability. A wire value the set does not name is a parse error the triage
/// agent corrects, exactly like an unknown ticket id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum TriageCategory {
    /// Work correcting behavior that already exists, including a regression.
    Bug,
    /// New behavior or capability.
    Enhancement,
}

impl TriageCategory {
    /// Every category, in the order they are documented.
    pub const ALL: [TriageCategory; 2] = [TriageCategory::Bug, TriageCategory::Enhancement];

    /// The JSON wire value of this category.
    pub fn wire(self) -> &'static str {
        match self {
            TriageCategory::Bug => "bug",
            TriageCategory::Enhancement => "enhancement",
        }
    }
}

/// What the triage pass decided about a draft ticket.
///
/// The set is closed and its four wire values are distinct. ready-for-agent is
/// the only state that lets the queue run the ticket; the other three each carry
/// their own reading -- a human decision, a missing answer, or work this run
/// will not do -- and each one blocks the whole set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum TriageState {
    /// The ticket is self-contained: the code agent can implement it now, so
    /// the classification carries an [AgentBrief] and no blocker explanation.
    ReadyForAgent,
    /// The ticket needs a human decision or human work before an agent can run
    /// it, so the classification carries a blocker explanation and no brief.
    ReadyForHuman,
    /// The ticket is under-specified and needs an answer from a human before an
    /// agent can run it, so the classification carries a blocker explanation and
    /// no brief.
    NeedsInfo,
    /// The ticket is redundant, already implemented or out of scope, so this run
    /// will not execute it: the classification carries a blocker explanation and
    /// no brief.
    Wontfix,
}

impl TriageState {
    /// Every state, in the order they are documented.
    pub const ALL: [TriageState; 4] = [
        TriageState::ReadyForAgent,
        TriageState::ReadyForHuman,
        TriageState::NeedsInfo,
        TriageState::Wontfix,
    ];

    /// The JSON wire value of this state.
    pub fn wire(self) -> &'static str {
        match self {
            TriageState::ReadyForAgent => "ready-for-agent",
            TriageState::ReadyForHuman => "ready-for-human",
            TriageState::NeedsInfo => "needs-info",
            TriageState::Wontfix => "wontfix",
        }
    }

    /// Whether this state carries an [AgentBrief] instead of a blocker
    /// explanation.
    pub fn is_ready(self) -> bool {
        matches!(self, TriageState::ReadyForAgent)
    }
}

/// The brief a ready-for-agent classification hands to the code agent.
///
/// The brief restates the draft ticket for the code agent; it never replaces the
/// ticket. Every field is required by serde and none has a default, and
/// validation rejects a blank target_behavior, an acceptance_criteria list
/// without at least one entry or with any blank entry, an out_of_scope list
/// without at least one entry or with any blank entry, and a blocked_by list
/// that differs from the draft ticket's.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AgentBrief {
    /// What the code agent must make true, in prose.
    pub target_behavior: String,
    /// Concrete, machine-verifiable pass/fail conditions. At least one entry,
    /// and every entry non-blank.
    pub acceptance_criteria: Vec<String>,
    /// What the code agent must not touch. At least one entry, and every entry
    /// non-blank: write the single entry None. when there is no exclusion.
    pub out_of_scope: Vec<String>,
    /// The draft ticket's blocked_by ids, unchanged.
    pub blocked_by: Vec<String>,
}

/// One draft ticket's classification.
///
/// The identity fields carry the draft identity contract: ticket_id and
/// ticket_title must equal the draft ticket's trimmed id and trimmed title
/// exactly. A padded, reworded or unknown identity is refused, and the record
/// carries the draft ticket's own identity rather than a candidate value the
/// model invented.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TriageTicket {
    /// The draft ticket's id, exactly as the draft identity contract carries it.
    pub ticket_id: String,
    /// The draft ticket's title, exactly as the draft identity contract carries
    /// it.
    pub ticket_title: String,
    /// What kind of change the ticket carries.
    pub category: TriageCategory,
    /// The state the triage pass decided on.
    pub state: TriageState,
    /// What was audited, one entry per finding about the ticket. At least one
    /// entry must be non-blank.
    pub notes: Vec<String>,
    /// Why the ticket is not ready for an agent. Required, and non-blank, for
    /// every state but ready-for-agent; absent for a ready ticket.
    #[serde(default)]
    pub blocker_explanation: Option<String>,
    /// The brief the code agent implements. Required for a ready-for-agent
    /// state; absent for every other state.
    #[serde(default)]
    pub agent_brief: Option<AgentBrief>,
}

/// The whole triage pass's typed handoff: one classification per draft ticket.
///
/// This is a handoff payload and a document, not an execution contract: the
/// queue executes the ticket files, and [TriageReport::to_markdown] renders
/// this report in draft-ticket order so the artifact reads as the draft set
/// reads whatever order the classifications arrive in.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TriageReport {
    /// The classifications, one per draft ticket, in the order the triage agent
    /// emitted them.
    pub tickets: Vec<TriageTicket>,
}

impl TriageReport {
    /// Validate every report invariant against the typed specification and the
    /// original draft set.
    ///
    /// The invariants are decided entirely by the draft set: the draft set must
    /// itself be non-empty and valid, the report must carry exactly as many
    /// classifications as the draft set has tickets, each draft ticket must be
    /// classified exactly once by a classification that carries the draft
    /// ticket's id and trimmed title exactly, records at least one non-blank
    /// audit note, and carries either an [AgentBrief] (for ready-for-agent)
    /// or a non-blank blocker explanation (for every other state) -- never both,
    /// never neither. A brief must state a non-blank target
    /// behavior, at least one non-blank acceptance criterion and at least one
    /// non-blank out-of-scope entry -- no blank entry survives in either list --
    /// and its dependencies must be the draft ticket's blocked_by ids, after
    /// trimming.
    ///
    /// The specification is the upstream half of this boundary and is
    /// deliberately not consulted: the draft tickets already carry the sliced
    /// id, title and dependency list a classification must preserve, and the
    /// specification's coverage is enforced where that boundary is drawn --
    /// finish_tickets refuses a set that drops a story or alters a criterion
    /// -- so re-deriving it here would refuse a triage report for a defect that
    /// is not the report's.
    pub fn validate_against(
        &self,
        _specification: &Specification,
        tickets: &TicketSet,
    ) -> Result<(), String> {
        if tickets.tickets.is_empty() {
            return Err(
                "[ERROR] the draft ticket set is empty; there is no draft set to triage, so no \
                 classification can be validated against it"
                    .to_string(),
            );
        }

        // The draft set a report is validated against must itself be valid, so a
        // report is never accepted for an input set the boundary would refuse.
        tickets.validate()?;

        // The report must classify the whole validated draft set: every draft
        // ticket exactly once, by a classification that carries the draft
        // ticket's own id. An unknown id, a duplicate and a missing draft
        // ticket are each reported with the counts, so the agent knows exactly
        // what to fix.
        let draft_ids: HashSet<&str> = tickets
            .tickets
            .iter()
            .map(|ticket| ticket.id.trim())
            .collect();

        let mut by_id: HashMap<&str, &TriageTicket> = HashMap::new();
        for classification in &self.tickets {
            // The submitted identity is used exactly as it arrived: a padded id
            // is not the draft ticket's id, and it is refused as an unknown id
            // rather than trimmed into a match.
            let id = classification.ticket_id.as_str();
            if id.trim().is_empty() {
                return Err(format!(
                    "[ERROR] the triage report carries {} classification(s) for {} draft \
                     ticket(s), and one of them has an empty ticket_id; every classification \
                     must carry the draft ticket's id",
                    self.tickets.len(),
                    tickets.tickets.len()
                ));
            }
            if !draft_ids.contains(id) {
                return Err(format!(
                    "[ERROR] the triage report carries {} classification(s) for {} draft \
                     ticket(s), and the id {id:?} is not a draft ticket id; every classification \
                     must carry the draft ticket's id exactly, with no padding, no rewording and \
                     no unknown id",
                    self.tickets.len(),
                    tickets.tickets.len()
                ));
            }
            if by_id.insert(id, classification).is_some() {
                return Err(format!(
                    "[ERROR] the triage report carries {} classification(s) for {} draft \
                     ticket(s), and the draft ticket {id} is classified twice; every draft ticket \
                     must be classified exactly once",
                    self.tickets.len(),
                    tickets.tickets.len()
                ));
            }
        }

        // The report must carry exactly one classification per draft ticket, so
        // its length must equal the validated draft set's. A report that is
        // short by a ticket names the draft ticket that has none.
        if self.tickets.len() != tickets.tickets.len() {
            let mut missing = String::new();
            for ticket in &tickets.tickets {
                let id = ticket.id.trim();
                if !by_id.contains_key(id) {
                    missing.push_str(&format!(
                        "; the draft ticket {id} has no triage classification"
                    ));
                }
            }
            return Err(format!(
                "[ERROR] the triage report carries {} classification(s) for {} draft ticket(s){}; \
                 the report must carry exactly one classification per draft ticket",
                self.tickets.len(),
                tickets.tickets.len(),
                missing
            ));
        }

        for ticket in &tickets.tickets {
            let draft_id = ticket.id.trim();
            // The length, unique-id and known-id rules above guarantee this
            // branch; it stays so the check below never guesses an identity.
            let Some(classification) = by_id.get(draft_id) else {
                return Err(format!(
                    "[ERROR] the triage report carries {} classification(s) for {} draft \
                     ticket(s), and the draft ticket {draft_id} has no triage classification; \
                     every draft ticket must be classified exactly once",
                    self.tickets.len(),
                    tickets.tickets.len()
                ));
            };
            check_classification(draft_id, ticket, classification, &draft_ids)?;
        }

        Ok(())
    }

    /// Render this report as the markdown document the pipeline writes to
    /// ticket-triage.md.
    ///
    /// The classifications are rendered in draft-ticket order, so a report whose
    /// classifications arrived in another order still reads as the draft set
    /// reads. The renderer is called on a report
    /// [TriageReport::validate_against] accepted: it walks the draft set, so a
    /// draft ticket with no classification is skipped rather than invented, and
    /// a classification the draft set does not carry is unreachable. Every field
    /// is trimmed and blank list entries are dropped, and the returned string
    /// ends with exactly one newline.
    pub fn to_markdown(&self, tickets: &TicketSet) -> String {
        let by_id: HashMap<&str, &TriageTicket> = self
            .tickets
            .iter()
            .map(|classification| (classification.ticket_id.as_str(), classification))
            .collect();

        let mut blocks = vec![TRIAGE_TITLE.to_string()];
        for ticket in &tickets.tickets {
            let id = ticket.id.trim();
            // A validated report classifies every draft ticket, so a draft
            // ticket without a classification is skipped rather than invented.
            let Some(classification) = by_id.get(id) else {
                continue;
            };
            blocks.push(classification_markdown(
                id,
                ticket.title.trim(),
                classification,
            ));
        }

        let mut document = blocks.join("\n\n");
        document.push('\n');
        document
    }
}

/// Validate one classification against the draft ticket it classifies.
///
/// The draft ticket is authoritative: the classification must carry its identity
/// exactly, record at least one non-blank audit note, and carry exactly one of
/// the two state-specific payloads -- an [AgentBrief] for ready-for-agent, a
/// non-blank blocker explanation for every other state.
fn check_classification(
    draft_id: &str,
    draft: &Ticket,
    classification: &TriageTicket,
    draft_ids: &HashSet<&str>,
) -> Result<(), String> {
    let submitted_title = classification.ticket_title.as_str();
    let draft_title = draft.title.trim();
    if submitted_title != draft_title {
        return Err(format!(
            "[ERROR] the triage classification for {draft_id} has the title \
             {submitted_title:?}, but the draft ticket's title is {draft_title:?}; every \
             classification must carry the draft ticket's title exactly, without padding or \
             rewording"
        ));
    }

    if !has_non_blank(&classification.notes) {
        return Err(format!(
            "[ERROR] the triage classification for {draft_id} has no non-blank audit note; every \
             classification must record at least one note that says what was audited"
        ));
    }

    if classification.state.is_ready() {
        if classification.blocker_explanation.is_some() {
            return Err(format!(
                "[ERROR] the triage classification for {draft_id} is {} but carries a blocker \
                 explanation; a ready-for-agent classification carries no explanation, so either \
                 drop it or record the state that explanation describes",
                classification.state.wire()
            ));
        }

        let Some(brief) = classification.agent_brief.as_ref() else {
            return Err(format!(
                "[ERROR] the triage classification for {draft_id} is {} without an agent_brief; \
                 every ready-for-agent classification must carry the Agent Brief the code agent \
                 implements -- the target behavior, at least one machine-verifiable acceptance \
                 criterion, at least one out-of-scope boundary, and the draft ticket's \
                 dependencies",
                classification.state.wire()
            ));
        };

        if brief.target_behavior.trim().is_empty() {
            return Err(format!(
                "[ERROR] the Agent Brief for {draft_id} has blank target_behavior; the brief must \
                 state the behavior the code agent must make true"
            ));
        }

        if brief.acceptance_criteria.is_empty() {
            return Err(format!(
                "[ERROR] the Agent Brief for {draft_id} has no acceptance_criteria entry; the \
                 brief must state at least one machine-verifiable criterion the verification gate \
                 can check"
            ));
        }

        if has_blank(&brief.acceptance_criteria) {
            return Err(format!(
                "[ERROR] the Agent Brief for {draft_id} has a blank acceptance_criteria entry; \
                 every acceptance criterion must be a non-blank, machine-verifiable condition the \
                 verification gate can check, so drop the blank entry or state the criterion it \
                 stood for"
            ));
        }

        if brief.out_of_scope.is_empty() {
            return Err(format!(
                "[ERROR] the Agent Brief for {draft_id} has no out_of_scope entry; every brief \
                 must name at least one boundary the code agent must not cross, and write {NONE} \
                 when there is no exclusion"
            ));
        }

        if has_blank(&brief.out_of_scope) {
            return Err(format!(
                "[ERROR] the Agent Brief for {draft_id} has a blank out_of_scope entry; every \
                 boundary the code agent must not cross must be non-blank, and a brief that \
                 excludes nothing writes the single entry {NONE}, so drop the blank entry or name \
                 the boundary it stood for"
            ));
        }

        return check_dependencies(draft_id, &draft.blocked_by, &brief.blocked_by, draft_ids);
    }

    if classification.agent_brief.is_some() {
        return Err(format!(
            "[ERROR] the triage classification for {draft_id} is {} but carries an agent_brief; \
             an Agent Brief belongs only to a ready-for-agent classification",
            classification.state.wire()
        ));
    }

    let explained = classification
        .blocker_explanation
        .as_deref()
        .is_some_and(|explanation| !explanation.trim().is_empty());
    if !explained {
        return Err(format!(
            "[ERROR] the triage classification for {draft_id} is {} without a blocker \
             explanation; every non-ready classification must explain what blocks the code agent \
             -- ready-for-human and needs-info say what must be answered, and wontfix says why \
             the run will not execute the ticket",
            classification.state.wire()
        ));
    }

    Ok(())
}

/// Validate a brief's dependencies against the draft ticket's blocked_by ids.
///
/// The brief must carry the draft ticket's dependencies unchanged, after
/// trimming: a dependency repeated, a dependency that is not a draft ticket, and
/// a dependency list that differs from the draft ticket's are all refused, so a
/// brief cannot silently widen or narrow what the queue must run first.
fn check_dependencies(
    draft_id: &str,
    declared: &[String],
    brief: &[String],
    draft_ids: &HashSet<&str>,
) -> Result<(), String> {
    let carried = trimmed_entries(brief);

    let mut seen: HashSet<&str> = HashSet::new();
    for dependency in &carried {
        if !seen.insert(*dependency) {
            return Err(format!(
                "[ERROR] the Agent Brief for {draft_id} lists the dependency {dependency} more \
                 than once; list every dependency once"
            ));
        }
    }

    for dependency in &carried {
        if !draft_ids.contains(dependency) {
            return Err(format!(
                "[ERROR] the Agent Brief for {draft_id} depends on {dependency}, which is not a \
                 draft ticket; every brief dependency must name a ticket of the draft set"
            ));
        }
    }

    let expected = trimmed_entries(declared);
    if carried != expected {
        return Err(format!(
            "[ERROR] the Agent Brief for {draft_id} carries the dependencies [{}], but the draft \
             ticket declares [{}]; a brief must carry the draft ticket's blocked_by ids unchanged",
            carried.join(", "),
            expected.join(", ")
        ));
    }

    Ok(())
}

/// The trimmed, non-blank entries of a list: what a renderer or a comparison
/// sees once the blank entries are dropped.
fn trimmed_entries(entries: &[String]) -> Vec<&str> {
    entries
        .iter()
        .map(|entry| entry.trim())
        .filter(|entry| !entry.is_empty())
        .collect()
}

/// Whether at least one entry of a list is non-blank.
fn has_non_blank(entries: &[String]) -> bool {
    entries.iter().any(|entry| !entry.trim().is_empty())
}

/// Whether any entry of a list is blank, entry by entry: one blank entry is
/// refused even when the list carries non-blank entries beside it.
fn has_blank(entries: &[String]) -> bool {
    entries.iter().any(|entry| entry.trim().is_empty())
}

/// One classification as a markdown block: the draft ticket's id and title, the
/// category, the state, the notes, the blocker explanation when it applies, and
/// the Agent Brief when the state carries one.
fn classification_markdown(id: &str, title: &str, classification: &TriageTicket) -> String {
    let mut block = format!("## {id}: {title}");
    block.push_str(&format!(
        "\n\n- **Category:** {}\n- **State:** {}",
        classification.category.wire(),
        classification.state.wire()
    ));

    let notes = trimmed_entries(&classification.notes);
    if notes.is_empty() {
        block.push_str(&format!("\n- **Notes:** {NONE}"));
    } else {
        block.push_str("\n- **Notes:**");
        for note in notes {
            block.push_str(&format!("\n  - {note}"));
        }
    }

    let explanation = classification
        .blocker_explanation
        .as_deref()
        .map(str::trim)
        .filter(|explanation| !explanation.is_empty());
    if let Some(explanation) = explanation {
        block.push_str(&format!("\n- **Blocker explanation:** {explanation}"));
    }

    if let Some(brief) = classification.agent_brief.as_ref() {
        block.push('\n');
        block.push_str(&brief_markdown(brief));
    }

    block
}

/// The Agent Brief as a markdown block, complete: nothing is elided, because
/// the brief is the part of the report the code agent reads.
fn brief_markdown(brief: &AgentBrief) -> String {
    let mut text = format!(
        "{BRIEF_HEADING}\n\n**Target behavior:** {}",
        brief.target_behavior.trim()
    );
    text.push_str(&format!(
        "\n\n**Acceptance criteria:**\n\n{}",
        bullets(&brief.acceptance_criteria)
    ));
    text.push_str(&format!(
        "\n\n**Out of scope:**\n\n{}",
        bullets(&brief.out_of_scope)
    ));
    text.push_str(&format!(
        "\n\n**Blocked by:** {}",
        dependency_line(&brief.blocked_by)
    ));
    text
}

/// A bullet-list body: one bullet per non-blank entry, or [NONE].
fn bullets(entries: &[String]) -> String {
    let bullets: Vec<String> = trimmed_entries(entries)
        .iter()
        .map(|entry| format!("- {entry}"))
        .collect();

    if bullets.is_empty() {
        NONE.to_string()
    } else {
        bullets.join("\n")
    }
}

/// The dependency ids as one line, each in backticks, or [NONE] when the draft
/// ticket declares none.
fn dependency_line(entries: &[String]) -> String {
    let ids: Vec<String> = trimmed_entries(entries)
        .iter()
        .map(|id| format!("\u{60}{id}\u{60}"))
        .collect();

    if ids.is_empty() {
        NONE.to_string()
    } else {
        ids.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::specification::{
        DeferredItems, ImplementationDecisions, Specification, TestingDecisions,
    };
    use crate::tickets::{SECTION_HEADINGS, Ticket};

    /// The specification the draft sets below were sliced from. Its content is
    /// empty on purpose: no report invariant is derived from it, because the
    /// draft tickets carry the id, title and dependencies a classification
    /// preserves.
    fn specification() -> Specification {
        Specification {
            feature_name: "Typed triage report".to_string(),
            problem_statement: "The draft ticket set has no readiness audit.".to_string(),
            solution_overview: "Classify the draft set as a typed report.".to_string(),
            user_stories: Vec::new(),
            implementation_decisions: ImplementationDecisions {
                module_architecture: "A new triage module owns the report.".to_string(),
                data_models_and_contracts: "One classification per draft ticket.".to_string(),
                adr_compliance: "Mirrors the typed review-report convention.".to_string(),
            },
            testing_decisions: TestingDecisions {
                test_seams: "TriageReport::validate_against.".to_string(),
                coverage_strategy: "Unit tests over the validator and the renderer.".to_string(),
                prior_art: "The mod tests block in review.rs.".to_string(),
            },
            out_of_scope: Vec::new(),
            deferred_items: DeferredItems {
                operational_risks: String::new(),
                follow_up_work: String::new(),
            },
        }
    }

    /// One draft ticket, carrying the fields the report contract preserves.
    fn draft_ticket(id: &str, title: &str, blocked_by: &[&str]) -> Ticket {
        Ticket {
            id: id.to_string(),
            blocked_by: blocked_by.iter().map(|id| id.to_string()).collect(),
            blocks: Vec::new(),
            title: title.to_string(),
            objective: format!("Deliver {title} end to end."),
            acceptance_criteria: vec![format!("{title} is implemented.")],
            prerequisites_assumed: "The crate builds.".to_string(),
            out_of_scope: Vec::new(),
            scope_of_changes: vec![format!("Change what {title} needs.")],
            test_expectations: vec!["when_running_the_slice_given_setup_then_passes".to_string()],
            verification_gate: "cargo test triage::tests".to_string(),
        }
    }

    /// The smallest draft set: one ticket that declares no dependency.
    fn one_ticket_draft() -> TicketSet {
        TicketSet {
            tickets: vec![draft_ticket("1-only", "Only slice", &[])],
        }
    }

    /// A draft set with one dependency edge between its two tickets.
    fn two_ticket_draft() -> TicketSet {
        TicketSet {
            tickets: vec![
                draft_ticket("1-first", "First slice", &[]),
                draft_ticket("2-second", "Second slice", &["1-first"]),
            ],
        }
    }

    /// A report carrying the given classifications, in the given order.
    fn report_of(tickets: Vec<TriageTicket>) -> TriageReport {
        TriageReport { tickets }
    }

    /// The classification shape the contract accepts for a ready ticket.
    fn ready_classification(id: &str, title: &str, blocked_by: &[&str]) -> TriageTicket {
        TriageTicket {
            ticket_id: id.to_string(),
            ticket_title: title.to_string(),
            category: TriageCategory::Enhancement,
            state: TriageState::ReadyForAgent,
            notes: vec!["The draft ticket is one self-contained slice.".to_string()],
            blocker_explanation: None,
            agent_brief: Some(AgentBrief {
                target_behavior: format!("Implement {title}."),
                acceptance_criteria: vec![format!("{id} is implemented.")],
                out_of_scope: vec![NONE.to_string()],
                blocked_by: blocked_by.iter().map(|id| id.to_string()).collect(),
            }),
        }
    }

    /// The classification shape the contract accepts for a non-ready ticket.
    fn blocked_classification(
        id: &str,
        title: &str,
        state: TriageState,
        explanation: &str,
    ) -> TriageTicket {
        TriageTicket {
            ticket_id: id.to_string(),
            ticket_title: title.to_string(),
            category: TriageCategory::Bug,
            state,
            notes: vec!["The draft ticket was audited against the draft set.".to_string()],
            blocker_explanation: Some(explanation.to_string()),
            agent_brief: None,
        }
    }

    // -- Core Entry-Point Behaviors ----------------------------------------

    #[test]
    fn when_rendering_triage_given_reordered_classifications_then_preserves_draft_order() {
        let tickets = two_ticket_draft();
        let report = report_of(vec![
            ready_classification("2-second", "Second slice", &["1-first"]),
            ready_classification("1-first", "First slice", &[]),
        ]);

        assert_eq!(report.validate_against(&specification(), &tickets), Ok(()));
        let markdown = report.to_markdown(&tickets);

        assert!(markdown.starts_with("# Ticket triage report\n"));
        assert!(
            markdown.ends_with("**Blocked by:** \u{60}1-first\u{60}\n"),
            "the report ends with the last draft ticket and exactly one newline: {markdown}"
        );

        let first = markdown
            .find("## 1-first: First slice")
            .expect("the first draft ticket has a heading");
        let second = markdown
            .find("## 2-second: Second slice")
            .expect("the second draft ticket has a heading");
        assert!(
            first < second,
            "the report must follow draft-ticket order, not the model's order: {markdown}"
        );

        let first_block = &markdown[first..second];
        assert!(first_block.contains("**Target behavior:** Implement First slice."));
        assert!(!first_block.contains("Implement Second slice."));
        assert!(first_block.contains("**Blocked by:** None."));

        let second_block = &markdown[second..];
        assert!(second_block.contains("**Target behavior:** Implement Second slice."));
        assert!(second_block.contains("**Blocked by:** \u{60}1-first\u{60}"));
    }

    // -- Edge-Case Tables ---------------------------------------------------

    #[test]
    fn when_validating_triage_given_state_field_combinations_then_accepts_only_contract() {
        let tickets = one_ticket_draft();
        let specification = specification();
        let ready = ready_classification("1-only", "Only slice", &[]);

        let report = report_of(vec![ready.clone()]);
        assert_eq!(
            report.validate_against(&specification, &tickets),
            Ok(()),
            "a ready classification carries a brief and no explanation"
        );

        let not_ready = blocked_classification(
            "1-only",
            "Only slice",
            TriageState::NeedsInfo,
            "The draft ticket names no test seam.",
        );
        let report = report_of(vec![not_ready.clone()]);
        assert_eq!(
            report.validate_against(&specification, &tickets),
            Ok(()),
            "a non-ready classification carries an explanation and no brief"
        );

        let mut ready_with_explanation = ready.clone();
        ready_with_explanation.blocker_explanation = Some("The ticket is blocked.".to_string());

        let mut ready_with_blank_explanation = ready.clone();
        ready_with_blank_explanation.blocker_explanation = Some("   \n".to_string());

        let mut ready_without_brief = ready.clone();
        ready_without_brief.agent_brief = None;

        let mut blank_target_behavior = ready.clone();
        blank_target_behavior
            .agent_brief
            .as_mut()
            .expect("the ready helper carries a brief")
            .target_behavior = "  \n".to_string();

        let mut blank_criteria = ready.clone();
        blank_criteria
            .agent_brief
            .as_mut()
            .expect("the ready helper carries a brief")
            .acceptance_criteria = vec!["  ".to_string(), "\t".to_string()];

        let mut blank_out_of_scope = ready.clone();
        blank_out_of_scope
            .agent_brief
            .as_mut()
            .expect("the ready helper carries a brief")
            .out_of_scope = vec!["   ".to_string()];

        let mut not_ready_without_explanation = not_ready.clone();
        not_ready_without_explanation.blocker_explanation = None;

        let mut not_ready_with_blank_explanation = not_ready.clone();
        not_ready_with_blank_explanation.blocker_explanation = Some("\n".to_string());

        let mut not_ready_with_brief = not_ready.clone();
        not_ready_with_brief.agent_brief = ready.agent_brief.clone();

        let cases: [(&str, TriageTicket, &str); 9] = [
            (
                "a ready classification with an explanation",
                ready_with_explanation,
                "blocker explanation",
            ),
            (
                "a ready classification with a blank explanation",
                ready_with_blank_explanation,
                "blocker explanation",
            ),
            (
                "a ready classification without a brief",
                ready_without_brief,
                "Agent Brief",
            ),
            (
                "a brief with blank target behavior",
                blank_target_behavior,
                "target_behavior",
            ),
            (
                "a brief with blank acceptance criteria",
                blank_criteria,
                "acceptance_criteria",
            ),
            (
                "a brief with a blank out-of-scope boundary",
                blank_out_of_scope,
                "out_of_scope",
            ),
            (
                "a non-ready classification without an explanation",
                not_ready_without_explanation,
                "explanation",
            ),
            (
                "a non-ready classification with a blank explanation",
                not_ready_with_blank_explanation,
                "explanation",
            ),
            (
                "a non-ready classification with a brief",
                not_ready_with_brief,
                "Agent Brief",
            ),
        ];

        for (case, classification, expected) in cases {
            let report = report_of(vec![classification]);
            let error = report
                .validate_against(&specification, &tickets)
                .expect_err(case);
            assert!(
                error.contains("1-only"),
                "{case}: must name the ticket: {error}"
            );
            assert!(
                error.contains(expected),
                "{case}: must name the field: {error}"
            );
        }
    }

    /// Every acceptance criterion must be non-blank: a brief whose list
    /// carries one valid criterion and one blank entry is refused exactly like
    /// a brief that states no criterion at all.
    #[test]
    fn when_validating_triage_given_mixed_blank_acceptance_criteria_then_rejects_report() {
        let tickets = one_ticket_draft();
        let specification = specification();

        let valid = ready_classification("1-only", "Only slice", &[]);
        assert_eq!(
            report_of(vec![valid.clone()]).validate_against(&specification, &tickets),
            Ok(()),
            "one non-blank acceptance criterion is the minimum the brief needs"
        );

        let cases: [(&str, Vec<String>); 3] = [
            (
                "a blank criterion before a valid one",
                vec!["  ".to_string(), "Only slice is implemented.".to_string()],
            ),
            (
                "a blank criterion after a valid one",
                vec!["Only slice is implemented.".to_string(), "\t\n".to_string()],
            ),
            (
                "a blank criterion between two valid ones",
                vec![
                    "Only slice is implemented.".to_string(),
                    " ".to_string(),
                    "The verification gate exits with code 0.".to_string(),
                ],
            ),
        ];

        for (case, criteria) in cases {
            let mut classification = valid.clone();
            classification
                .agent_brief
                .as_mut()
                .expect("the ready helper carries a brief")
                .acceptance_criteria = criteria;

            let error = report_of(vec![classification])
                .validate_against(&specification, &tickets)
                .expect_err(case);
            assert!(
                error.contains("1-only"),
                "{case}: must name the ticket: {error}"
            );
            assert!(
                error.contains("acceptance_criteria"),
                "{case}: must name the field: {error}"
            );
        }
    }

    /// Every out-of-scope entry must be non-blank too: a brief whose boundary
    /// list carries one valid entry and one blank entry is refused, so a blank
    /// exclusion can no longer ride along beside a real one.
    #[test]
    fn when_validating_triage_given_mixed_blank_out_of_scope_then_rejects_report() {
        let tickets = one_ticket_draft();
        let specification = specification();

        let valid = ready_classification("1-only", "Only slice", &[]);
        assert_eq!(
            report_of(vec![valid.clone()]).validate_against(&specification, &tickets),
            Ok(()),
            "one non-blank out-of-scope entry is the minimum the brief needs"
        );

        let cases: [(&str, Vec<String>); 3] = [
            (
                "a blank boundary before a valid one",
                vec!["  ".to_string(), NONE.to_string()],
            ),
            (
                "a blank boundary after a valid one",
                vec!["src/pipeline.rs".to_string(), "\n".to_string()],
            ),
            (
                "a blank boundary between two valid ones",
                vec![
                    "src/pipeline.rs".to_string(),
                    "\t ".to_string(),
                    "src/orchestrator.rs".to_string(),
                ],
            ),
        ];

        for (case, boundaries) in cases {
            let mut classification = valid.clone();
            classification
                .agent_brief
                .as_mut()
                .expect("the ready helper carries a brief")
                .out_of_scope = boundaries;

            let error = report_of(vec![classification])
                .validate_against(&specification, &tickets)
                .expect_err(case);
            assert!(
                error.contains("1-only"),
                "{case}: must name the ticket: {error}"
            );
            assert!(
                error.contains("out_of_scope"),
                "{case}: must name the field: {error}"
            );
        }
    }

    // -- Contract & Invariant Checks ---------------------------------------

    #[test]
    fn when_validating_triage_given_duplicate_missing_or_unknown_ticket_then_rejects_report() {
        let tickets = two_ticket_draft();
        let specification = specification();

        let mut nameless = ready_classification("2-second", "Second slice", &["1-first"]);
        nameless.ticket_id = "  ".to_string();
        let error = report_of(vec![
            ready_classification("1-first", "First slice", &[]),
            nameless,
        ])
        .validate_against(&specification, &tickets)
        .expect_err("an empty classification id must be rejected");
        assert!(error.contains("empty ticket_id"), "{error}");

        let error = report_of(vec![
            ready_classification("1-first", "First slice", &[]),
            ready_classification("9-absent", "Absent slice", &[]),
        ])
        .validate_against(&specification, &tickets)
        .expect_err("an unknown classification id must be rejected");
        assert!(error.contains("9-absent"), "{error}");
        assert!(error.contains("not a draft ticket"), "{error}");

        let error = report_of(vec![
            ready_classification("1-first", "First slice", &[]),
            ready_classification("1-first", "First slice", &[]),
        ])
        .validate_against(&specification, &tickets)
        .expect_err("a duplicated classification must be rejected");
        assert!(error.contains("1-first"), "{error}");
        assert!(error.contains("twice"), "{error}");
        assert!(error.contains("exactly once"), "{error}");

        let error = report_of(vec![ready_classification("1-first", "First slice", &[])])
            .validate_against(&specification, &tickets)
            .expect_err("a missing classification must be rejected");
        assert!(error.contains("2-second"), "{error}");
        assert!(error.contains("no triage classification"), "{error}");
    }

    // -- Regression Tests ---------------------------------------------------

    /// The whole ticket document one draft ticket renders to, written out here
    /// independently of the renderer: the regression assertion compares against
    /// this document, so a rendering change that is not also made here fails
    /// instead of matching itself.
    fn expected_ticket_document() -> &'static str {
        r#"---
id: "1-first"
blocked_by: []
blocks: []
---

# 1-first: First slice

## Objective

Deliver First slice end to end.

## Acceptance Criteria

- First slice is implemented.

## Context & Boundaries

**Prerequisites assumed:** The crate builds.

**Out of scope for this ticket:**

None.

## Scope of Changes

- Change what First slice needs.

## Test Expectations

- when_running_the_slice_given_setup_then_passes

## Verification Gate

The exact terminal command below must exit with code 0:

```bash
cargo test triage::tests
```
"#
    }

    #[test]
    fn when_rendering_triage_given_original_ticket_then_preserves_ticket_document() {
        let ticket = draft_ticket("1-first", "First slice", &[]);
        let document = ticket.to_markdown();
        let tickets = TicketSet {
            tickets: vec![ticket.clone()],
        };
        let report = report_of(vec![ready_classification("1-first", "First slice", &[])]);

        assert_eq!(report.validate_against(&specification(), &tickets), Ok(()));
        let triage = report.to_markdown(&tickets);

        assert_eq!(
            ticket.to_markdown(),
            expected_ticket_document(),
            "rendering a triage report must not change the ticket renderer"
        );
        assert_eq!(ticket.file_name(), "1-first.md");
        assert!(
            document.starts_with("---\nid: \"1-first\"\n"),
            "the ticket document keeps its front matter: {document}"
        );
        for heading in SECTION_HEADINGS {
            assert!(
                !triage.contains(heading),
                "the triage document must not restate the ticket's {heading} section: {triage}"
            );
        }
        assert!(
            !triage.contains("id: \"1-first\""),
            "the triage document is not a ticket document: {triage}"
        );
        assert_ne!(triage, document);
        assert!(
            triage.contains("## 1-first: First slice"),
            "the triage document names the ticket it classifies: {triage}"
        );
    }

    // -- Empty/Zero/Nil -----------------------------------------------------

    #[test]
    fn when_validating_triage_given_empty_report_for_nonempty_set_then_rejects_report() {
        let tickets = one_ticket_draft();
        let specification = specification();

        let report = report_of(Vec::new());
        let error = report
            .validate_against(&specification, &tickets)
            .expect_err("an empty report must not be accepted");
        assert!(error.contains("1-only"), "{error}");
        assert!(error.contains("no triage classification"), "{error}");

        let nothing = TicketSet {
            tickets: Vec::new(),
        };
        let error = report
            .validate_against(&specification, &nothing)
            .expect_err("a draft set with no tickets has nothing to triage");
        assert!(error.contains("draft ticket set is empty"), "{error}");

        for payload in [
            "null",
            "{}",
            r#"{"tickets":null}"#,
            r#"{"tickets":[null]}"#,
            r#"{"classifications":[]}"#,
        ] {
            assert!(
                serde_json::from_str::<TriageReport>(payload).is_err(),
                "{payload} must not deserialize into a report"
            );
        }

        let empty = serde_json::from_str::<TriageReport>(r#"{"tickets":[]}"#)
            .expect("an explicitly empty classification list is a report");
        assert!(empty.tickets.is_empty());
    }

    // -- Minimum valid boundary --------------------------------------------

    #[test]
    fn when_validating_triage_given_one_ticket_minimum_brief_then_accepts_report() {
        let tickets = one_ticket_draft();
        let report = report_of(vec![TriageTicket {
            ticket_id: "1-only".to_string(),
            ticket_title: "Only slice".to_string(),
            category: TriageCategory::Bug,
            state: TriageState::ReadyForAgent,
            notes: vec!["x".to_string()],
            blocker_explanation: None,
            agent_brief: Some(AgentBrief {
                target_behavior: "x".to_string(),
                acceptance_criteria: vec!["y".to_string()],
                out_of_scope: vec![NONE.to_string()],
                blocked_by: Vec::new(),
            }),
        }]);

        assert_eq!(report.validate_against(&specification(), &tickets), Ok(()));

        let markdown = report.to_markdown(&tickets);
        assert!(markdown.contains("## 1-only: Only slice"));
        assert!(markdown.contains("- **Category:** bug"));
        assert!(markdown.contains("- **State:** ready-for-agent"));
        assert!(markdown.contains("- **Notes:**\n  - x"));
        assert!(markdown.contains("**Target behavior:** x"));
        assert!(markdown.contains("- y"));
        assert!(markdown.contains("**Out of scope:**\n\n- None."));
        assert!(markdown.contains("**Blocked by:** None."));
        assert!(!markdown.contains("blocker explanation"));
    }

    // -- Maximum limit / Upper boundary ------------------------------------

    #[test]
    fn when_validating_triage_given_multi_ticket_fixture_then_preserves_all_classifications_and_dependencies()
     {
        let tickets = TicketSet {
            tickets: vec![
                draft_ticket("1-first", "First slice", &[]),
                draft_ticket("2-second", "Second slice", &["1-first"]),
                draft_ticket("3-third", "Third slice", &["2-second"]),
            ],
        };
        let blocked = blocked_classification(
            "2-second",
            "Second slice",
            TriageState::ReadyForHuman,
            "The draft ticket names no test seam.",
        );
        let report = report_of(vec![
            ready_classification("3-third", "Third slice", &["2-second"]),
            blocked,
            ready_classification("1-first", "First slice", &[]),
        ]);

        assert_eq!(report.validate_against(&specification(), &tickets), Ok(()));
        assert_eq!(
            report.tickets.len(),
            tickets.tickets.len(),
            "every classification survives validation"
        );

        let markdown = report.to_markdown(&tickets);
        let first = markdown
            .find("## 1-first: First slice")
            .expect("the first ticket renders");
        let second = markdown
            .find("## 2-second: Second slice")
            .expect("the second ticket renders");
        let third = markdown
            .find("## 3-third: Third slice")
            .expect("the third ticket renders");
        assert!(first < second && second < third, "{markdown}");

        assert!(markdown.contains("**Blocked by:** None."));
        assert!(
            markdown.contains("**Blocked by:** \u{60}2-second\u{60}"),
            "the largest fixture keeps its dependencies: {markdown}"
        );

        let blocked_block = &markdown[second..third];
        assert!(
            blocked_block.contains("- **State:** ready-for-human"),
            "{blocked_block}"
        );
        assert!(
            blocked_block
                .contains("- **Blocker explanation:** The draft ticket names no test seam."),
            "{blocked_block}"
        );
        assert!(
            !blocked_block.contains(BRIEF_HEADING),
            "a non-ready classification carries no brief: {blocked_block}"
        );

        assert!(
            markdown[first..second].contains(BRIEF_HEADING),
            "a ready classification keeps its brief: {markdown}"
        );
        assert!(markdown[third..].contains(BRIEF_HEADING));
    }

    // -- Malformed/Invalid payload -----------------------------------------

    #[test]
    fn when_validating_triage_given_malformed_fields_or_dependency_mismatch_then_rejects_report() {
        let tickets = two_ticket_draft();
        let specification = specification();
        let second = ready_classification("2-second", "Second slice", &["1-first"]);

        let mut wrong_title = second.clone();
        wrong_title.ticket_title = "Second slice, reworded".to_string();

        let mut no_notes = second.clone();
        no_notes.notes.clear();

        let mut blank_notes = second.clone();
        blank_notes.notes = vec!["  ".to_string(), "\t\n".to_string()];

        let mut repeated_dependency = second.clone();
        repeated_dependency
            .agent_brief
            .as_mut()
            .expect("the ready helper carries a brief")
            .blocked_by = vec!["1-first".to_string(), "1-first".to_string()];

        let mut unknown_dependency = second.clone();
        unknown_dependency
            .agent_brief
            .as_mut()
            .expect("the ready helper carries a brief")
            .blocked_by = vec!["1-first".to_string(), "9-absent".to_string()];

        let mut dropped_dependency = second.clone();
        dropped_dependency
            .agent_brief
            .as_mut()
            .expect("the ready helper carries a brief")
            .blocked_by
            .clear();

        let mut extra_dependency = second.clone();
        extra_dependency
            .agent_brief
            .as_mut()
            .expect("the ready helper carries a brief")
            .blocked_by = vec!["1-first".to_string(), "2-second".to_string()];

        let mut non_ready_with_brief = second.clone();
        non_ready_with_brief.state = TriageState::Wontfix;
        non_ready_with_brief.blocker_explanation = Some("Nothing to execute.".to_string());

        let cases: [(&str, TriageTicket, &str, &str); 8] = [
            (
                "a title that is not the draft ticket's",
                wrong_title,
                "2-second",
                "title",
            ),
            ("no audit note at all", no_notes, "2-second", "note"),
            (
                "audit notes that are blank",
                blank_notes,
                "2-second",
                "note",
            ),
            (
                "a repeated dependency",
                repeated_dependency,
                "2-second",
                "more than once",
            ),
            (
                "an unknown dependency",
                unknown_dependency,
                "9-absent",
                "not a draft ticket",
            ),
            (
                "a dropped dependency",
                dropped_dependency,
                "2-second",
                "dependencies",
            ),
            (
                "an extra dependency",
                extra_dependency,
                "2-second",
                "dependencies",
            ),
            (
                "a non-ready classification that carries a brief",
                non_ready_with_brief,
                "2-second",
                "Agent Brief",
            ),
        ];

        for (case, classification, named, expected) in cases {
            let report = report_of(vec![
                ready_classification("1-first", "First slice", &[]),
                classification,
            ]);
            let error = report
                .validate_against(&specification, &tickets)
                .expect_err(case);
            assert!(error.contains(named), "{case}: must name {named}: {error}");
            assert!(
                error.contains(expected),
                "{case}: must name the field: {error}"
            );
        }
    }

    // -- Boundary unauthorized/Forbidden access ----------------------------

    #[test]
    fn when_validating_triage_given_no_authorization_contract_then_records_not_applicable() {
        assert!(
            AUTHORIZATION_BOUNDARY.starts_with("Not applicable"),
            "{AUTHORIZATION_BOUNDARY}"
        );
        assert!(
            AUTHORIZATION_BOUNDARY.contains("no user authorization boundary"),
            "{AUTHORIZATION_BOUNDARY}"
        );
        assert!(
            AUTHORIZATION_BOUNDARY.contains("read-only tool capabilities"),
            "{AUTHORIZATION_BOUNDARY}"
        );
        assert!(
            AUTHORIZATION_BOUNDARY.contains("agent/tool ticket"),
            "{AUTHORIZATION_BOUNDARY}"
        );
    }

    // -- Wire contract ------------------------------------------------------

    #[test]
    fn when_serializing_triage_given_closed_contract_enums_then_emits_exact_values() {
        assert_eq!(
            TriageCategory::ALL,
            [TriageCategory::Bug, TriageCategory::Enhancement]
        );
        for category in TriageCategory::ALL {
            assert_eq!(
                serde_json::to_value(category).expect("a category serializes"),
                serde_json::json!(category.wire())
            );
            assert!(
                matches!(category.wire(), "bug" | "enhancement"),
                "the wire value is exactly bug or enhancement: {}",
                category.wire()
            );
        }

        assert_eq!(
            TriageState::ALL,
            [
                TriageState::ReadyForAgent,
                TriageState::ReadyForHuman,
                TriageState::NeedsInfo,
                TriageState::Wontfix,
            ]
        );
        for state in TriageState::ALL {
            assert_eq!(
                serde_json::to_value(state).expect("a state serializes"),
                serde_json::json!(state.wire())
            );
            assert!(
                !state.wire().contains('_'),
                "the states are hyphenated wire values, never underscore-separated: {}",
                state.wire()
            );
            assert_eq!(state.is_ready(), state == TriageState::ReadyForAgent);
        }

        for wire in [
            "ready-for-agent",
            "ready-for-human",
            "needs-info",
            "wontfix",
        ] {
            assert_eq!(
                serde_json::from_str::<TriageState>(&format!("{wire:?}"))
                    .expect("a wire state parses")
                    .wire(),
                wire
            );
        }
        for wire in ["bug", "enhancement"] {
            assert_eq!(
                serde_json::from_str::<TriageCategory>(&format!("{wire:?}"))
                    .expect("a wire category parses")
                    .wire(),
                wire
            );
        }

        for payload in [
            "feature",
            "refactor",
            "test",
            "docs",
            "chore",
            "fix",
            "improvement",
            "",
        ] {
            assert!(
                serde_json::from_str::<TriageCategory>(&format!("{payload:?}")).is_err(),
                "{payload:?} is not a wire category"
            );
        }
        for payload in [
            "ready_for_agent",
            "ready_for_human",
            "needs_info",
            "needs-clarification",
            "not-actionable",
            "",
        ] {
            assert!(
                serde_json::from_str::<TriageState>(&format!("{payload:?}")).is_err(),
                "{payload:?} is not a wire state"
            );
        }
    }

    #[test]
    fn when_validating_triage_given_untrimmed_identity_fields_then_rejects_mismatch() {
        let tickets = one_ticket_draft();
        let specification = specification();
        let clean = ready_classification("1-only", "Only slice", &[]);

        assert_eq!(
            report_of(vec![clean.clone()]).validate_against(&specification, &tickets),
            Ok(()),
            "the draft identity contract is the trimmed draft id and title"
        );

        let mut leading_id = clean.clone();
        leading_id.ticket_id = " 1-only".to_string();
        let error = report_of(vec![leading_id])
            .validate_against(&specification, &tickets)
            .expect_err("a padded ticket id is not the draft ticket's id");
        assert!(error.contains("1-only"), "{error}");
        assert!(error.contains("not a draft ticket"), "{error}");

        let mut trailing_id = clean.clone();
        trailing_id.ticket_id = "1-only ".to_string();
        let error = report_of(vec![trailing_id])
            .validate_against(&specification, &tickets)
            .expect_err("a padded ticket id is not the draft ticket's id");
        assert!(error.contains("not a draft ticket"), "{error}");

        for padded_title in [" Only slice", "Only slice ", "Only  slice"] {
            let mut classification = clean.clone();
            classification.ticket_title = padded_title.to_string();
            let error = report_of(vec![classification])
                .validate_against(&specification, &tickets)
                .expect_err("a padded title is not the draft ticket's title");
            assert!(error.contains("title"), "{error}");
        }

        for padded_id in [" 1-only", "1-only ", "1-only\n"] {
            let mut classification = clean.clone();
            classification.ticket_id = padded_id.to_string();
            let error = report_of(vec![classification])
                .validate_against(&specification, &tickets)
                .expect_err("an untrimmed submitted id is refused, never padded into a match");
            assert!(error.contains("not a draft ticket"), "{error}");
        }
    }

    #[test]
    fn when_deserializing_triage_given_wire_payload_then_carries_every_field() {
        let payload = serde_json::json!({
            "tickets": [
                {
                    "ticket_id": "1-first",
                    "ticket_title": "First slice",
                    "category": "enhancement",
                    "state": "ready-for-agent",
                    "notes": ["The draft ticket is one self-contained slice."],
                    "agent_brief": {
                        "target_behavior": "Implement First slice.",
                        "acceptance_criteria": ["First slice is implemented."],
                        "out_of_scope": ["None."],
                        "blocked_by": []
                    }
                },
                {
                    "ticket_id": "2-second",
                    "ticket_title": "Second slice",
                    "category": "bug",
                    "state": "needs-info",
                    "notes": ["The draft ticket names no test seam."],
                    "blocker_explanation": "The draft ticket names no test seam."
                }
            ]
        })
        .to_string();

        let report: TriageReport =
            serde_json::from_str(&payload).expect("the payload is a triage report");
        assert_eq!(report.tickets.len(), 2);
        assert_eq!(report.tickets[0].category, TriageCategory::Enhancement);
        assert_eq!(report.tickets[0].state, TriageState::ReadyForAgent);
        assert!(
            report.tickets[0].blocker_explanation.is_none(),
            "an absent explanation is no explanation"
        );
        assert_eq!(report.tickets[1].category, TriageCategory::Bug);
        assert_eq!(report.tickets[1].state, TriageState::NeedsInfo);
        assert!(
            report.tickets[1].agent_brief.is_none(),
            "an absent brief is no brief"
        );
        assert_eq!(
            report.validate_against(&specification(), &two_ticket_draft()),
            Ok(())
        );

        // The identity fields are the contract's names, not the old candidate
        // names: an id/title payload is not a triage ticket.
        for payload in [
            r#"{"tickets":[{"id":"1-first","title":"First slice","category":"bug","state":"wontfix","notes":["x"],"blocker_explanation":"y"}]}"#,
            r#"{"tickets":[{"ticket_id":"1-first","ticket_title":"First slice","category":"feature","state":"ready-for-agent","notes":["x"]}]}"#,
        ] {
            assert!(
                serde_json::from_str::<TriageReport>(payload).is_err(),
                "{payload} must not deserialize into a report"
            );
        }
    }
}
