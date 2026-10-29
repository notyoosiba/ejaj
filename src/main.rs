use ejaj::Config;
use ejaj::pipeline::{Pipeline, Step};
use ejaj::status;
use rustyline::error::ReadlineError;
use rustyline::{Cmd, DefaultEditor, EventHandler, KeyCode, KeyEvent, Modifiers};
use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, exit};
use std::time::{SystemTime, UNIX_EPOCH};

/// Load `.env` into the process environment, keeping dotenvy's rule that an
/// already-set variable is never overwritten, but reporting keys that `.env`
/// defines more than once. With two provider blocks the first definition wins,
/// which otherwise looks like the agent ignoring the provider you configured.
fn load_dotenv() {
    let Some(iter) = dotenv_pairs() else {
        return;
    };

    let mut seen: Vec<String> = Vec::new();
    for item in iter {
        if !load_dotenv_item(item, &mut seen) {
            return;
        }
    }
}

/// The iterator over .env pairs, or none after the load failure is reported.
/// Keeping the fallible open here leaves the loading loop itself branch-light.
fn dotenv_pairs() -> Option<impl Iterator<Item = Result<(String, String), dotenvy::Error>>> {
    match dotenvy::from_filename_iter(".env") {
        Ok(iter) => Some(iter),
        Err(e) => {
            log_dotenv_debug("Could not load .env file", &e);
            None
        }
    }
}

/// Report one .env loading diagnostic without letting a failed log write
/// disturb startup.
fn log_dotenv_debug(context: &str, error: &dyn std::fmt::Display) {
    let _ = status::shared().stderr_log(&format!("[DEBUG] {}: {}", context, error));
}

/// Apply one parsed .env pair. A parse failure is reported and stops the load,
/// exactly as a failed load does. A duplicate key keeps the first value and
/// reports the existing warning. Returns false when loading must stop.
fn load_dotenv_item(
    item: Result<(String, String), dotenvy::Error>,
    seen: &mut Vec<String>,
) -> bool {
    let Some(pair) = parsed_dotenv_pair(item) else {
        return false;
    };
    let (pair, warning) = track_dotenv_pair(pair, seen);
    if let Some(warning) = warning {
        let _ = status::shared().stderr_log(&warning);
    }
    apply_dotenv_pair(pair);
    true
}

/// A parsed pair, or none after the parse failure is reported.
fn parsed_dotenv_pair(item: Result<(String, String), dotenvy::Error>) -> Option<(String, String)> {
    match item {
        Ok(pair) => Some(pair),
        Err(e) => {
            log_dotenv_debug("Could not parse .env file", &e);
            None
        }
    }
}

/// Write one pair into the process environment unless the variable is already
/// set, which is dotenvy's established precedence.
fn apply_dotenv_pair((key, value): (String, String)) {
    if env::var_os(&key).is_none() {
        // Safe: single-threaded, before any other thread is spawned.
        unsafe { env::set_var(&key, value) };
    }
}

/// Record one successfully parsed `.env` pair in `seen`, returning the pair
/// unchanged for environment application plus the existing duplicate warning
/// when this key was already defined. The first definition wins because the
/// caller only writes a key the process environment does not already hold, so a
/// later definition is reported but not applied.
fn track_dotenv_pair(
    pair: (String, String),
    seen: &mut Vec<String>,
) -> ((String, String), Option<String>) {
    let key = pair.0.clone();
    let warning = if seen.contains(&key) {
        Some(format!(
            "[WARN] .env sets {} more than once; the first value wins and later ones are \
             ignored. Comment out the provider block you are not using.",
            key
        ))
    } else {
        None
    };
    seen.push(key);
    (pair, warning)
}

/// A directory name for one handoff. Prefers `date +%Y%m%d-%H%M%S` (local time,
/// human-readable and lexically sortable) through the same `Command` pattern
/// [`project_root`] uses, so no new dependency is needed. If `date` is missing,
/// fails, or returns nothing, fall back to seconds since the Unix epoch so a
/// directory can always be named.
fn plan_timestamp() -> String {
    let from_date = Command::new("date")
        .arg("+%Y%m%d-%H%M%S")
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|stamp| !stamp.is_empty());
    if let Some(stamp) = from_date {
        return stamp;
    }

    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(since_epoch) => since_epoch.as_secs().to_string(),
        Err(_) => "0".to_string(),
    }
}

/// The directory for one handoff, `.ejaj/<timestamp>/`. All artifacts of that
/// handoff -- `plan.md`, `spec.md`, `ticket-triage.md`, the end-of-run
/// `queue-report.md`, `review.md` and `change-summary.md`, and the per-ticket
/// files under `tickets/` -- land here, so a single directory holds the whole
/// exchange.
fn handoff_dir() -> PathBuf {
    Path::new(".ejaj").join(plan_timestamp())
}

/// Resolve the one directory the agent is allowed to work in: the git top
/// level that owns the current working directory, falling back to the cwd when
/// not inside a repo. Handing this to the model removes the guesswork that made
/// it wander the disk in the first place.
fn project_root() -> String {
    Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|path| !path.is_empty())
        .or_else(|| {
            env::current_dir()
                .ok()
                .map(|path| path.display().to_string())
        })
        .unwrap_or_else(|| ".".to_string())
}

/// Appended to every system prompt: the concrete project root plus the rule
/// that everything the agent reads or searches must live under it. The process
/// working directory is pinned to the root in [`main`], so the model is told to
/// take that for granted and use paths relative to the project root without a
/// `cd <root> &&` prefix.
fn scope_notice(root: &str) -> String {
    format!(
        "\n\nThe project root is `{root}`. Work only inside it: never read, list, or \
         search outside this directory (`/`, `~`, `/etc`, sibling projects), and never \
         inspect `target/` or `.git/`. The working directory is already set to the project \
         root, so always use paths relative to `{root}` and never prefix a command with \
         `cd {root} &&`.\n"
    )
}

/// The REPL's Enter bindings, in one definition so `main` registers exactly
/// this mapping and no second, contradictory one can appear: plain Enter
/// submits the prompt, Alt+Enter inserts a soft newline.
fn repl_keybindings() -> [(KeyEvent, Cmd); 2] {
    [
        (KeyEvent(KeyCode::Enter, Modifiers::NONE), Cmd::AcceptLine),
        (KeyEvent(KeyCode::Enter, Modifiers::ALT), Cmd::Newline),
    ]
}

/// Build the validated configuration and report what it resolved to, in the
/// order the process has always used.
fn configured() -> Config {
    let config = Config::from_env();
    config.validate();
    let _ = status::shared().stdout_log(&format!("[INFO] {}", config.describe()));
    config
}

/// Resolve the one project root and make it the process working directory.
/// A root that no longer exists is fatal: running commands against the wrong
/// directory is worse than stopping. The diagnostic and exit are unchanged.
fn enter_project_root() -> String {
    let root = project_root();
    if let Err(e) = env::set_current_dir(&root) {
        let _ = status::shared().stderr_log(&format!(
            "[ERROR] Could not set the working directory to the project root {}: {}",
            root, e
        ));
        exit(1);
    }
    root
}

/// The real line editor: arrow keys move the cursor, Up/Down walk the history,
/// Ctrl-C abandons the current line and Ctrl-D exits. Without it, raw ANSI
/// escape sequences (e.g. ^[[C) would be inserted as literal text. A failed
/// initialization keeps its existing fatal diagnostic and exit status.
fn line_editor() -> DefaultEditor {
    match DefaultEditor::new() {
        Ok(rl) => rl,
        Err(e) => {
            let _ = status::shared().stderr_log(&format!(
                "[ERROR] Could not initialize the line editor: {}",
                e
            ));
            exit(1);
        }
    }
}

/// Register the one Enter mapping: plain Enter submits, Alt+Enter inserts a
/// soft newline.
fn bind_repl_keys(rl: &mut DefaultEditor) {
    for (key, cmd) in repl_keybindings() {
        rl.bind_sequence(key, EventHandler::Simple(cmd));
    }
}

/// The prompt readline reports back: a submitted line, the two terminal stop
/// signals, or another input failure. Keeping them distinct lets the REPL keep,
/// end, or stop itself without inspecting raw errors in the loop.
#[derive(Debug)]
enum ReplRead {
    Line(String),
    Interrupted,
    Eof,
    Failed(ReadlineError),
}

/// Classify one readline result without side effects.
fn classify_readline(result: Result<String, ReadlineError>) -> ReplRead {
    match result {
        Ok(line) => ReplRead::Line(line),
        Err(ReadlineError::Interrupted) => ReplRead::Interrupted,
        Err(ReadlineError::Eof) => ReplRead::Eof,
        Err(e) => ReplRead::Failed(e),
    }
}

/// What one prompt cycle asks the REPL loop to do next.
#[derive(Debug, PartialEq, Eq)]
enum ReplAction {
    Continue,
    Exit,
}

/// The nonempty trimmed input, or none for blank and whitespace-only lines.
/// Blank lines are not history entries and never reach the pipeline.
fn normalized_input(line: &str) -> Option<&str> {
    let input = line.trim();
    if input.is_empty() { None } else { Some(input) }
}

/// The fatal stderr line for a pipeline outcome, or none for the two outcomes
/// that keep or end the REPL normally. Retry and Complete are not failures.
fn fatal_message(step: &Step) -> Option<String> {
    if let Step::WriteFailed { artifact, error } = step {
        return Some(format!(
            "[ERROR] Could not write the {} to .ejaj: {}",
            artifact, error
        ));
    }
    if let Step::SummaryFailed { reason } = step {
        return Some(format!("[ERROR] Could not summarise the run: {}", reason));
    }
    if let Step::GitFailed { stage, reason } = step {
        return Some(git_failure_message(stage, reason));
    }
    None
}

/// Git failures share one shape, except the preflight that refuses to start
/// the run and therefore gets its own wording.
fn git_failure_message(stage: &str, reason: &str) -> String {
    if stage == "preflight" {
        format!("[ERROR] Cannot start the run: Git preflight failed: {reason}")
    } else {
        format!("[ERROR] Could not {stage} the run's changes: {reason}")
    }
}

/// One prompt cycle: read, classify, and handle the line or the stop.
fn repl_action(rl: &mut DefaultEditor, pipeline: &mut Pipeline) -> ReplAction {
    match classify_readline(rl.readline("plan>> ")) {
        ReplRead::Line(line) => handle_line(rl, pipeline, &line),
        ReplRead::Interrupted => ReplAction::Continue,
        ReplRead::Eof => ReplAction::Exit,
        ReplRead::Failed(e) => {
            let _ = status::shared().stderr_log(&format!("[ERROR] Input error: {}", e));
            ReplAction::Exit
        }
    }
}

/// Add one nonblank trimmed line to history and hand it to the pipeline once.
fn handle_line(rl: &mut DefaultEditor, pipeline: &mut Pipeline, line: &str) -> ReplAction {
    let Some(input) = normalized_input(line) else {
        return ReplAction::Continue;
    };
    let _ = rl.add_history_entry(input);
    handle_step(pipeline.ask(input, &handoff_dir()))
}

/// Log the one fatal line a failed step maps to and exit, or keep going for a
/// retry / stop for completion.
fn handle_step(step: Step) -> ReplAction {
    if let Some(line) = fatal_message(&step) {
        let _ = status::shared().stderr_log(&line);
        exit(1);
    }
    match step {
        Step::Complete => ReplAction::Exit,
        _ => ReplAction::Continue,
    }
}

/// The interactive loop: every cycle either returns to the prompt or ends it.
fn run_repl(rl: &mut DefaultEditor, pipeline: &mut Pipeline) {
    loop {
        if repl_action(rl, pipeline) == ReplAction::Exit {
            break;
        }
    }
}

fn main() {
    load_dotenv();

    let config = configured();

    // The agent works inside one directory. Resolve it once and hand it to
    // every system prompt so the model never has to guess where "here" is --
    // and never wanders outside it. Entering it here keeps the established
    // startup order and the exact fatal diagnostic.
    let root = enter_project_root();
    let scope = scope_notice(&root);

    // One pipeline for the process: the plan agent, the spec and tickets passes,
    // the triage pass, the ticket queue, the review pass and the change-summary
    // pass. The plan agent's transcript lives for the whole process: the
    // interview continues across every REPL line. Each handoff runs one fresh
    // spec pass, one fresh tickets pass and one fresh triage pass, writing
    // plan.md, spec.md, ticket-triage.md and one file per ticket into the same
    // .ejaj/<timestamp>/ directory -- nothing is carried over from an earlier
    // pass. The triage pass reads the original draft set and never changes it;
    // the code agent is driven only by the queue, gets a fresh transcript seeded
    // with one ticket's own rendered text, and nothing is carried between
    // tickets. No transcript is ever replayed under another agent's system
    // prompt: every handoff payload crosses its boundary as a typed object --
    // the plan at the plan/spec boundary, the specification at the spec/tickets
    // boundary, the ticket set at the tickets/queue boundary and the triage
    // report at the triage/queue boundary -- and is rendered to markdown only
    // inside the pipeline, where the documents are produced.
    // The resolved root travels with the pipeline: every invocation runs its
    // Git preflight there first, so no agent starts on a tree that is not
    // clean. project_root() falls back to the cwd when it cannot find a
    // repository, but that fallback is a best-effort root, not a statement that
    // a non-repository directory is valid: the preflight refuses one.
    let mut pipeline = Pipeline::new(&config, &scope, &root);

    // A real line editor and the one Enter mapping from repl_keybindings.
    let mut rl = line_editor();
    bind_repl_keys(&mut rl);

    // The prompt is always plan>> : every user line goes to the plan agent.
    // A stopped queue starts again only through a new finish_plan on a revised
    // plan, which runs a fresh spec pass, a fresh tickets pass and the queue
    // once more, so the prompt never lies about who is listening.
    run_repl(&mut rl, &mut pipeline);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    #[test]
    fn when_tracking_given_first_dotenv_key_then_records_pair_without_warning() {
        let mut seen: Vec<String> = Vec::new();

        let (pair, warning) =
            track_dotenv_pair(("KEY".to_string(), "first".to_string()), &mut seen);

        assert_eq!(
            pair,
            ("KEY".to_string(), "first".to_string()),
            "the parsed pair is preserved for environment application"
        );
        assert_eq!(warning, None, "a first definition is not a duplicate");
        assert_eq!(
            seen,
            vec!["KEY".to_string()],
            "the first definition is recorded as seen"
        );
    }

    #[test]
    fn when_tracking_given_repeated_dotenv_key_then_returns_warning_and_updates_seen_keys() {
        let mut seen: Vec<String> = vec!["KEY".to_string()];

        let (pair, warning) =
            track_dotenv_pair(("KEY".to_string(), "second".to_string()), &mut seen);

        assert_eq!(
            pair,
            ("KEY".to_string(), "second".to_string()),
            "the parsed pair is preserved for environment application"
        );
        assert_eq!(
            warning.as_deref(),
            Some(
                "[WARN] .env sets KEY more than once; the first value wins and later ones are \
                 ignored. Comment out the provider block you are not using."
            ),
            "a repeated definition reports the existing duplicate warning"
        );
        assert_eq!(
            seen,
            vec!["KEY".to_string(), "KEY".to_string()],
            "every successful occurrence is recorded as seen"
        );
    }

    #[test]
    fn when_pressing_enter_given_plain_or_alt_modifier_then_submits_or_inserts_soft_newline() {
        let bindings = repl_keybindings();

        assert_eq!(
            bindings,
            [
                (KeyEvent(KeyCode::Enter, Modifiers::NONE), Cmd::AcceptLine),
                (KeyEvent(KeyCode::Enter, Modifiers::ALT), Cmd::Newline),
            ],
            "plain Enter submits the prompt and Alt+Enter inserts a soft newline"
        );
        assert_eq!(bindings.len(), 2, "exactly two bindings, in order");
        assert_eq!(
            bindings[0],
            (KeyEvent(KeyCode::Enter, Modifiers::NONE), Cmd::AcceptLine),
            "plain Enter submits the prompt"
        );
        assert_eq!(
            bindings[1],
            (KeyEvent(KeyCode::Enter, Modifiers::ALT), Cmd::Newline),
            "Alt+Enter inserts a soft newline"
        );
    }

    #[test]
    fn when_formatting_given_each_fatal_pipeline_outcome_then_returns_the_expected_error() {
        let cases = [
            (
                Step::WriteFailed {
                    artifact: "plan",
                    error: io::Error::other("disk full"),
                },
                "[ERROR] Could not write the plan to .ejaj: disk full",
            ),
            (
                Step::SummaryFailed {
                    reason: "the agent returned nothing".to_string(),
                },
                "[ERROR] Could not summarise the run: the agent returned nothing",
            ),
            (
                Step::GitFailed {
                    stage: "preflight",
                    reason: "the working tree is dirty".to_string(),
                },
                "[ERROR] Cannot start the run: Git preflight failed: the working tree is dirty",
            ),
            (
                Step::GitFailed {
                    stage: "commit",
                    reason: "nothing was staged".to_string(),
                },
                "[ERROR] Could not commit the run's changes: nothing was staged",
            ),
            (
                Step::GitFailed {
                    stage: "push",
                    reason: "the branch could not be published".to_string(),
                },
                "[ERROR] Could not push the run's changes: the branch could not be published",
            ),
        ];

        for (step, expected) in cases {
            assert_eq!(
                fatal_message(&step).as_deref(),
                Some(expected),
                "unexpected fatal message for {expected:?}"
            );
        }
    }

    #[test]
    fn when_finishing_given_retry_or_completion_then_has_no_fatal_message() {
        assert_eq!(
            fatal_message(&Step::Retry),
            None,
            "a retry returns to the prompt without a fatal line"
        );
        assert_eq!(
            fatal_message(&Step::Complete),
            None,
            "completion exits without a fatal line"
        );
    }

    #[test]
    fn when_prompting_given_a_submitted_line_then_preserves_the_entered_text() {
        match classify_readline(Ok("hello".to_string())) {
            ReplRead::Line(line) => assert_eq!(line, "hello"),
            other => panic!("expected a submitted line, got {other:?}"),
        }
    }

    #[test]
    fn when_prompting_given_interruption_or_end_of_input_then_starts_fresh_or_exits() {
        assert!(matches!(
            classify_readline(Err(ReadlineError::Interrupted)),
            ReplRead::Interrupted
        ));
        assert!(matches!(
            classify_readline(Err(ReadlineError::Eof)),
            ReplRead::Eof
        ));
    }

    #[test]
    fn when_prompting_given_another_input_error_then_preserves_it_for_reporting() {
        let classified = classify_readline(Err(ReadlineError::Io(io::Error::other("broken pipe"))));

        match classified {
            ReplRead::Failed(error) => assert!(error.to_string().contains("broken pipe")),
            other => panic!("expected an input failure, got {other:?}"),
        }
    }

    #[test]
    fn when_entering_given_an_empty_or_whitespace_line_then_ignores_it() {
        for line in ["", "   ", "\t\n "] {
            assert_eq!(
                normalized_input(line),
                None,
                "blank input {line:?} must be ignored"
            );
        }
    }

    #[test]
    fn when_entering_given_surrounded_text_then_uses_the_trimmed_prompt() {
        assert_eq!(
            normalized_input("  finish the plan  "),
            Some("finish the plan"),
            "the caller sees the trimmed line"
        );
        assert_eq!(
            normalized_input("\twake up\n"),
            Some("wake up"),
            "surrounding whitespace is removed"
        );
    }
}
