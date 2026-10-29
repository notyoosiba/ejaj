//! Binary-boundary startup tests for the canonical mid/giga configuration.
//!
//! Every case starts the real `ejaj` executable in its own empty scratch
//! directory with a cleared environment, so no `.env` and no ambient provider
//! variable can leak into the test. Standard input is closed and no REPL line is
//! ever supplied, so startup reaches its description (or its fatal diagnostic)
//! without making a provider call.
//!
//! The old `worker`/`architect` role names and the old generic provider
//! variables are exercised here as rejected input, never as aliases: the
//! canonical `mid`, `giga` and `hybrid` names are the only configuration the
//! binary accepts.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

/// The real binary Cargo built for this integration test.
const BINARY: &str = env!("CARGO_BIN_EXE_ejaj");

const MID_URL: &str = "http://mid.test/v1/chat/completions";
const MID_MODEL: &str = "mid-model";
const GIGA_URL: &str = "http://giga.test/v1/responses";
const GIGA_MODEL: &str = "giga-model";
const MID_HTTPS: &str = "https://mid.test/v1/chat/completions";
const GIGA_HTTPS: &str = "https://giga.test/v1/responses";
const SMOLL_URL: &str = "https://smoll.test/v1/messages";
const SMOLL_MODEL: &str = "smoll-model";
const SMOLL_DEFAULT_URL: &str = "https://api.anthropic.com/v1/messages";

/// A unique empty directory that disappears when the test ends.
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new() -> Scratch {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "ejaj-config-cli-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&path).expect("the scratch directory is created");
        Scratch { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// One startup's complete observation: the exit status and both streams.
struct Run {
    status: ExitStatus,
    stdout: String,
    stderr: String,
}

impl Run {
    /// The fatal lines, exactly as the process wrote them.
    fn error_lines(&self) -> Vec<&str> {
        self.stderr
            .lines()
            .filter(|line| line.trim_start().starts_with("[ERROR]"))
            .collect()
    }

    fn assert_success(&self) {
        assert!(
            self.status.success(),
            "startup failed (status {:?})\nstdout:\n{}\nstderr:\n{}",
            self.status.code(),
            self.stdout,
            self.stderr
        );
    }

    fn assert_reports(&self, expected: &str) {
        assert!(
            self.stdout.contains(expected),
            "missing startup description `{expected}`\nstdout:\n{}",
            self.stdout
        );
    }
}

/// Start the real binary once, with only the supplied environment variables
/// after a full clear, in a fresh directory and with closed stdin.
fn start(envs: &[(&str, &str)]) -> Run {
    let scratch = Scratch::new();
    let mut command = Command::new(BINARY);
    command
        .current_dir(scratch.path())
        .env_clear()
        .env(
            "PATH",
            std::env::var_os("PATH").unwrap_or_else(|| "/usr/local/bin:/usr/bin:/bin".into()),
        )
        .env("HOME", scratch.path())
        .env("TERM", "dumb")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (name, value) in envs {
        command.env(name, value);
    }

    let output = command.output().expect("the ejaj binary starts");
    Run {
        status: output.status,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// Assert the one-line fatal contract for an unsuccessful startup.
fn assert_one_error(run: &Run) -> &str {
    assert_eq!(
        run.status.code(),
        Some(1),
        "startup must exit 1\nstdout:\n{}\nstderr:\n{}",
        run.stdout,
        run.stderr
    );
    let errors = run.error_lines();
    assert_eq!(
        errors.len(),
        1,
        "startup must print exactly one [ERROR] line\nstderr:\n{}",
        run.stderr
    );
    errors[0]
}

/// The required Smoll coordinates, added to a startup that intends to exercise
/// some other behavior.
fn smoll_environment() -> Vec<(&'static str, &'static str)> {
    vec![
        ("LLM_SMOLL_MODEL", SMOLL_MODEL),
        ("LLM_SMOLL_API_KEY", "sk-smoll"),
    ]
}

/// The given startup variables plus a complete Smoll block.
fn with_smoll(envs: &[(&'static str, &'static str)]) -> Vec<(&'static str, &'static str)> {
    envs.iter().copied().chain(smoll_environment()).collect()
}

/// Start once with a complete Smoll block supplied, so the case can reach the
/// behavior it is actually about.
fn start_with_smoll(envs: &[(&'static str, &'static str)]) -> Run {
    start(&with_smoll(envs))
}

/// The existing mode requirements, with no Smoll values: mid needs nothing,
/// giga and hybrid need both giga coordinates.
fn mode_requirements(mode: &'static str) -> Vec<(&'static str, &'static str)> {
    let mut envs = vec![("AGENT_MODE", mode)];
    if mode != "mid" {
        envs.push(("LLM_GIGA_URL", GIGA_URL));
        envs.push(("LLM_GIGA_MODEL", GIGA_MODEL));
    }
    envs
}

/// Every canonical variable in one complete environment block.
fn full_environment(mode: &'static str) -> Vec<(&'static str, &'static str)> {
    vec![
        ("AGENT_MODE", mode),
        ("LLM_MID_URL", MID_HTTPS),
        ("LLM_MID_MODEL", MID_MODEL),
        ("LLM_MID_API_KEY", "sk-mid"),
        ("LLM_MID_AUTH", "bearer"),
        ("LLM_MID_REASONING_EFFORT", "max"),
        ("LLM_GIGA_URL", GIGA_HTTPS),
        ("LLM_GIGA_MODEL", GIGA_MODEL),
        ("LLM_GIGA_API_KEY", "sk-giga"),
        ("LLM_GIGA_AUTH", "bearer"),
        ("LLM_GIGA_REASONING_EFFORT", "low"),
    ]
}

fn mid_description() -> String {
    format!(
        "mode mid: plan/spec/tickets/triage/code/review -> chat-completions ({MID_MODEL}), summary -> anthropic-messages ({SMOLL_MODEL})"
    )
}

fn giga_description() -> String {
    format!(
        "mode giga: plan/spec/tickets/triage/code/review -> responses ({GIGA_MODEL}), summary -> anthropic-messages ({SMOLL_MODEL})"
    )
}

fn hybrid_description() -> String {
    format!(
        "mode hybrid: plan/spec/tickets/triage/review -> responses ({GIGA_MODEL}), code -> chat-completions ({MID_MODEL}), summary -> anthropic-messages ({SMOLL_MODEL})"
    )
}

/// Core entry-point behavior: each canonical mode starts, reports its own mode
/// and its resolved protocol/model routing, and mixed case is normalized.
#[test]
fn when_starting_given_canonical_provider_environment_then_reports_resolved_configuration() {
    let mid = start_with_smoll(&[
        ("AGENT_MODE", "  MiD  "),
        ("LLM_MID_URL", MID_URL),
        ("LLM_MID_MODEL", MID_MODEL),
    ]);
    mid.assert_success();
    mid.assert_reports(&mid_description());

    let giga = start_with_smoll(&[
        ("AGENT_MODE", "GIGA"),
        ("LLM_GIGA_URL", GIGA_URL),
        ("LLM_GIGA_MODEL", GIGA_MODEL),
    ]);
    giga.assert_success();
    giga.assert_reports(&giga_description());

    let hybrid = start_with_smoll(&[
        ("AGENT_MODE", "HyBrId"),
        ("LLM_MID_URL", MID_URL),
        ("LLM_MID_MODEL", MID_MODEL),
        ("LLM_GIGA_URL", GIGA_URL),
        ("LLM_GIGA_MODEL", GIGA_MODEL),
    ]);
    hybrid.assert_success();
    hybrid.assert_reports(&hybrid_description());
}

/// Edge-case table: the old role names are not modes. Each fails the same way
/// with exit 1, exactly one error line, the trimmed value and canonical
/// guidance.
#[test]
fn when_starting_given_legacy_mode_then_rejects_it_with_one_error_line() {
    for raw in ["worker", "architect", " worker ", " ARCHITECT "] {
        let run = start_with_smoll(&[("AGENT_MODE", raw)]);
        let error = assert_one_error(&run);
        let trimmed = raw.trim();

        assert!(
            error.contains("use mid, giga or hybrid"),
            "missing canonical mode guidance: {error}"
        );
        assert!(
            error.contains(&format!("AGENT_MODE=\"{trimmed}\"")),
            "the trimmed value {trimmed:?} is missing: {error}"
        );
        assert!(
            error.contains("not recognised"),
            "the value must be rejected as unknown: {error}"
        );
    }
}

/// Malformed/invalid payload: any other nonblank mode value takes the same
/// fatal path, and the trimmed value is the one reported.
#[test]
fn when_starting_given_unknown_nonblank_mode_then_rejects_it_with_one_error_line() {
    for raw in ["mystery", "  deepseek  ", "midi"] {
        let run = start_with_smoll(&[("AGENT_MODE", raw)]);
        let error = assert_one_error(&run);
        let trimmed = raw.trim();

        assert!(
            error.contains("use mid, giga or hybrid"),
            "missing canonical mode guidance: {error}"
        );
        assert!(
            error.contains(&format!("AGENT_MODE=\"{trimmed}\"")),
            "the trimmed value {trimmed:?} is missing: {error}"
        );
    }
}

/// Contract: old generic and old architect variables are never read as
/// fallbacks. They cannot override mid defaults, satisfy the giga requirement,
/// or satisfy an https endpoint's key or auth requirement.
#[test]
fn when_configuring_given_legacy_provider_variables_then_ignores_them() {
    let generic = start_with_smoll(&[
        ("AGENT_MODE", "mid"),
        (
            "LLM_PROVIDER_URL",
            "https://legacy.test/v1/chat/completions",
        ),
        ("LLM_MODEL", "legacy-model"),
        ("LLM_API_KEY", "legacy-key"),
        ("LLM_AUTH", "none"),
        ("LLM_REASONING_EFFORT", "max"),
    ]);
    generic.assert_success();
    assert!(
        generic.stdout.contains("chat-completions (local-model)"),
        "old generic variables must not override the mid defaults\nstdout:\n{}",
        generic.stdout
    );

    let architect_ignored = start_with_smoll(&[
        ("AGENT_MODE", "mid"),
        ("LLM_ARCHITECT_URL", "https://legacy.test/v1/responses"),
        ("LLM_ARCHITECT_MODEL", "legacy-giga-model"),
        ("LLM_ARCHITECT_API_KEY", "legacy-key"),
        ("LLM_ARCHITECT_AUTH", "none"),
        ("LLM_ARCHITECT_REASONING_EFFORT", "max"),
    ]);
    architect_ignored.assert_success();
    assert!(
        architect_ignored
            .stdout
            .contains("chat-completions (local-model)"),
        "old architect variables must not configure mid\nstdout:\n{}",
        architect_ignored.stdout
    );

    for mode in ["giga", "hybrid"] {
        let run = start_with_smoll(&[
            ("AGENT_MODE", mode),
            ("LLM_ARCHITECT_URL", GIGA_URL),
            ("LLM_ARCHITECT_MODEL", GIGA_MODEL),
            ("LLM_ARCHITECT_API_KEY", "legacy-key"),
            ("LLM_ARCHITECT_AUTH", "none"),
        ]);
        let error = assert_one_error(&run);

        assert!(
            error.contains("LLM_GIGA_URL") && error.contains("LLM_GIGA_MODEL"),
            "old architect variables must not satisfy {mode}: {error}"
        );
        assert!(
            !error.contains("LLM_ARCHITECT_"),
            "the diagnostic still names an old variable: {error}"
        );
    }

    let mid_legacy_key = start_with_smoll(&[
        ("AGENT_MODE", "mid"),
        ("LLM_MID_URL", MID_HTTPS),
        ("LLM_API_KEY", "legacy-key"),
        ("LLM_AUTH", "none"),
        ("LLM_ARCHITECT_API_KEY", "legacy-key"),
        ("LLM_ARCHITECT_AUTH", "none"),
    ]);
    let error = assert_one_error(&mid_legacy_key);
    assert!(
        error.contains("remote mid endpoint"),
        "the mid https endpoint must still demand a canonical key: {error}"
    );
    assert!(
        mid_legacy_key.stderr.contains("LLM_MID_API_KEY"),
        "a legacy key must not satisfy the mid endpoint:\n{}",
        mid_legacy_key.stderr
    );

    let giga_legacy_key = start_with_smoll(&[
        ("AGENT_MODE", "giga"),
        ("LLM_GIGA_URL", GIGA_HTTPS),
        ("LLM_GIGA_MODEL", GIGA_MODEL),
        ("LLM_API_KEY", "legacy-key"),
        ("LLM_AUTH", "none"),
        ("LLM_ARCHITECT_API_KEY", "legacy-key"),
        ("LLM_ARCHITECT_AUTH", "none"),
    ]);
    let error = assert_one_error(&giga_legacy_key);
    assert!(
        error.contains("remote giga endpoint"),
        "the giga https endpoint must still demand a canonical key: {error}"
    );
    assert!(
        giga_legacy_key.stderr.contains("LLM_GIGA_API_KEY"),
        "a legacy key must not satisfy the giga endpoint:\n{}",
        giga_legacy_key.stderr
    );
}

/// Regression: keyless HTTP endpoints remain accepted in every mode.
#[test]
fn when_starting_given_http_keyless_provider_then_preserves_existing_acceptance() {
    let mid = start_with_smoll(&[
        ("AGENT_MODE", "mid"),
        ("LLM_MID_URL", MID_URL),
        ("LLM_MID_MODEL", MID_MODEL),
    ]);
    mid.assert_success();
    mid.assert_reports(&mid_description());

    let giga = start_with_smoll(&[
        ("AGENT_MODE", "giga"),
        ("LLM_GIGA_URL", GIGA_URL),
        ("LLM_GIGA_MODEL", GIGA_MODEL),
    ]);
    giga.assert_success();
    giga.assert_reports(&giga_description());

    let hybrid = start_with_smoll(&[
        ("AGENT_MODE", "hybrid"),
        ("LLM_MID_URL", MID_URL),
        ("LLM_MID_MODEL", MID_MODEL),
        ("LLM_GIGA_URL", GIGA_URL),
        ("LLM_GIGA_MODEL", GIGA_MODEL),
    ]);
    hybrid.assert_success();
    hybrid.assert_reports(&hybrid_description());
}

/// Empty/zero/nil boundary: unset, empty and whitespace-only modes are blank,
/// not unknown modes, and resolve to the mid mode's local defaults.
#[test]
fn when_starting_given_unset_or_blank_mode_then_defaults_to_mid() {
    let unset = start_with_smoll(&[]);
    unset.assert_success();
    unset.assert_reports(
        "mode mid: plan/spec/tickets/triage/code/review -> chat-completions (local-model), summary -> anthropic-messages (smoll-model)",
    );

    for raw in ["", "   ", "\t \n"] {
        let run = start_with_smoll(&[("AGENT_MODE", raw)]);
        run.assert_success();
        assert!(
            run.stdout.contains("mode mid:"),
            "blank AGENT_MODE={raw:?} must resolve to mid\nstdout:\n{}",
            run.stdout
        );
        assert!(
            run.stdout.contains("chat-completions (local-model)"),
            "blank AGENT_MODE={raw:?} must keep the mid defaults\nstdout:\n{}",
            run.stdout
        );
    }
}

/// Blank canonical optional values behave as unset, so the local defaults and
/// keyless local use survive even when the variables are present.
#[test]
fn when_starting_given_blank_optional_values_then_uses_local_defaults() {
    let run = start_with_smoll(&[
        ("AGENT_MODE", "mid"),
        ("LLM_MID_URL", "   "),
        ("LLM_MID_MODEL", "\t "),
        ("LLM_MID_API_KEY", "   "),
        ("LLM_MID_AUTH", "   "),
        ("LLM_MID_REASONING_EFFORT", "   "),
        ("LLM_GIGA_URL", "\t"),
        ("LLM_GIGA_MODEL", "   "),
    ]);

    run.assert_success();
    run.assert_reports(
        "mode mid: plan/spec/tickets/triage/code/review -> chat-completions (local-model), summary -> anthropic-messages (smoll-model)",
    );
}

/// Minimum valid boundary: the two giga coordinates alone are enough to start,
/// in giga and in hybrid mode, with no key or auth override.
#[test]
fn when_starting_given_only_required_giga_coordinates_then_succeeds() {
    for mode in ["giga", "hybrid"] {
        let run = start_with_smoll(&[
            ("AGENT_MODE", mode),
            ("LLM_GIGA_URL", GIGA_URL),
            ("LLM_GIGA_MODEL", GIGA_MODEL),
        ]);
        run.assert_success();
        assert!(
            run.stdout.contains(&format!("mode {mode}:")),
            "minimal {mode} startup must report its mode\nstdout:\n{}",
            run.stdout
        );
    }
}

/// Upper boundary: a complete canonical environment block resolves in every
/// mode, and the resolved routing names the models that were supplied.
#[test]
fn when_starting_given_every_canonical_variable_then_resolves_all_settings() {
    let mid = start_with_smoll(&full_environment("mid"));
    mid.assert_success();
    mid.assert_reports(&mid_description());

    let giga = start_with_smoll(&full_environment("giga"));
    giga.assert_success();
    giga.assert_reports(&giga_description());

    let hybrid = start_with_smoll(&full_environment("hybrid"));
    hybrid.assert_success();
    hybrid.assert_reports(&hybrid_description());
}

/// Malformed/invalid payload: a giga block carrying exactly one coordinate is
/// fatal in every mode and names the canonical variable that is missing.
#[test]
fn when_starting_given_half_written_giga_coordinates_then_names_missing_variable() {
    for mode in ["mid", "giga", "hybrid"] {
        let url_only = start_with_smoll(&[("AGENT_MODE", mode), ("LLM_GIGA_URL", GIGA_URL)]);
        let error = assert_one_error(&url_only);
        assert_eq!(
            error,
            "[ERROR] The giga provider is half-configured: LLM_GIGA_MODEL is not set. Set it, or unset the other one.",
            "wrong diagnostic for {mode} with only LLM_GIGA_URL"
        );
        assert!(
            !error.contains("LLM_ARCHITECT_"),
            "the diagnostic names an old variable: {error}"
        );

        let model_only = start_with_smoll(&[("AGENT_MODE", mode), ("LLM_GIGA_MODEL", GIGA_MODEL)]);
        let error = assert_one_error(&model_only);
        assert_eq!(
            error,
            "[ERROR] The giga provider is half-configured: LLM_GIGA_URL is not set. Set it, or unset the other one.",
            "wrong diagnostic for {mode} with only LLM_GIGA_MODEL"
        );
        assert!(
            !error.contains("LLM_ARCHITECT_"),
            "the diagnostic names an old variable: {error}"
        );
    }
}

/// Malformed/invalid payload: giga and hybrid modes with no giga block name
/// both required canonical coordinates.
#[test]
fn when_starting_given_absent_giga_coordinates_then_requires_both_canonical_variables() {
    for mode in ["giga", "hybrid"] {
        let run = start_with_smoll(&[("AGENT_MODE", mode)]);
        let error = assert_one_error(&run);

        assert!(
            error.contains("LLM_GIGA_URL"),
            "LLM_GIGA_URL missing from the {mode} diagnostic: {error}"
        );
        assert!(
            error.contains("LLM_GIGA_MODEL"),
            "LLM_GIGA_MODEL missing from the {mode} diagnostic: {error}"
        );
        assert!(
            !error.contains("LLM_ARCHITECT_") && !error.contains("LLM_PROVIDER_"),
            "the diagnostic still names an old variable: {error}"
        );
    }
}

/// Unauthorized/forbidden boundary: a keyless https provider fails with the
/// actual canonical role and that role's own variables, while a key or
/// `AUTH=none` lets the same endpoint start.
#[test]
fn when_starting_given_keyless_https_mid_or_giga_then_reports_role_specific_hint() {
    let mid = start_with_smoll(&[("AGENT_MODE", "mid"), ("LLM_MID_URL", MID_HTTPS)]);
    let error = assert_one_error(&mid);
    assert!(error.contains("remote mid endpoint"), "wrong role: {error}");
    assert!(error.contains(MID_HTTPS), "wrong endpoint: {error}");
    assert!(
        mid.stderr
            .contains("Set LLM_MID_API_KEY, or set LLM_MID_AUTH=none for a keyless endpoint."),
        "missing mid key/auth hint:\n{}",
        mid.stderr
    );
    assert!(
        !mid.stderr.contains("LLM_GIGA_") && !mid.stderr.contains("LLM_ARCHITECT_"),
        "a mid error must not mention another role's variables:\n{}",
        mid.stderr
    );

    let giga = start_with_smoll(&[
        ("AGENT_MODE", "giga"),
        ("LLM_GIGA_URL", GIGA_HTTPS),
        ("LLM_GIGA_MODEL", GIGA_MODEL),
    ]);
    let error = assert_one_error(&giga);
    assert!(
        error.contains("remote giga endpoint"),
        "wrong role: {error}"
    );
    assert!(error.contains(GIGA_HTTPS), "wrong endpoint: {error}");
    assert!(
        giga.stderr
            .contains("Set LLM_GIGA_API_KEY, or set LLM_GIGA_AUTH=none for a keyless endpoint."),
        "missing giga key/auth hint:\n{}",
        giga.stderr
    );
    assert!(
        !giga.stderr.contains("LLM_MID_") && !giga.stderr.contains("LLM_ARCHITECT_"),
        "a giga error must not mention another role's variables:\n{}",
        giga.stderr
    );

    let hybrid_mid_keyless = start_with_smoll(&[
        ("AGENT_MODE", "hybrid"),
        ("LLM_MID_URL", MID_HTTPS),
        ("LLM_GIGA_URL", GIGA_URL),
        ("LLM_GIGA_MODEL", GIGA_MODEL),
    ]);
    let error = assert_one_error(&hybrid_mid_keyless);
    assert!(
        error.contains("remote mid endpoint"),
        "hybrid mode validates the mid block first: {error}"
    );

    let hybrid_giga_keyless = start_with_smoll(&[
        ("AGENT_MODE", "hybrid"),
        ("LLM_MID_URL", MID_HTTPS),
        ("LLM_MID_API_KEY", "sk-mid"),
        ("LLM_GIGA_URL", GIGA_HTTPS),
        ("LLM_GIGA_MODEL", GIGA_MODEL),
    ]);
    let error = assert_one_error(&hybrid_giga_keyless);
    assert!(
        error.contains("remote giga endpoint"),
        "hybrid mode validates the giga block too: {error}"
    );

    let keyed_mid = start_with_smoll(&[
        ("AGENT_MODE", "mid"),
        ("LLM_MID_URL", MID_HTTPS),
        ("LLM_MID_API_KEY", "sk-test"),
    ]);
    keyed_mid.assert_success();

    let keyed_giga = start_with_smoll(&[
        ("AGENT_MODE", "giga"),
        ("LLM_GIGA_URL", GIGA_HTTPS),
        ("LLM_GIGA_MODEL", GIGA_MODEL),
        ("LLM_GIGA_API_KEY", "sk-test"),
    ]);
    keyed_giga.assert_success();

    let keyless_but_disabled_mid = start_with_smoll(&[
        ("AGENT_MODE", "mid"),
        ("LLM_MID_URL", MID_HTTPS),
        ("LLM_MID_AUTH", " NONE "),
    ]);
    keyless_but_disabled_mid.assert_success();

    let keyless_but_disabled_giga = start_with_smoll(&[
        ("AGENT_MODE", "giga"),
        ("LLM_GIGA_URL", GIGA_HTTPS),
        ("LLM_GIGA_MODEL", GIGA_MODEL),
        ("LLM_GIGA_AUTH", " NONE "),
    ]);
    keyless_but_disabled_giga.assert_success();
}

/// Edge-case table: in every mode, a missing, empty or whitespace-only Smoll
/// model or key fails startup with exit 1 and one [ERROR] line that names the
/// missing canonical variable and its configuration guidance.
#[test]
fn when_resolving_given_missing_empty_or_whitespace_smoll_coordinates_then_names_the_missing_variable()
 {
    for mode in ["mid", "giga", "hybrid"] {
        for missing in ["LLM_SMOLL_MODEL", "LLM_SMOLL_API_KEY"] {
            for blank in [None, Some(""), Some("   "), Some("\t \n")] {
                let mut envs = mode_requirements(mode);
                if missing == "LLM_SMOLL_MODEL" {
                    envs.push(("LLM_SMOLL_API_KEY", "sk-smoll"));
                } else {
                    envs.push(("LLM_SMOLL_MODEL", SMOLL_MODEL));
                }
                if let Some(value) = blank {
                    envs.push((missing, value));
                }

                let run = start(&envs);
                let error = assert_one_error(&run);
                assert!(error.contains(missing), "mode {mode}: {error}");
                assert!(error.contains("LLM_SMOLL_MODEL"), "mode {mode}: {error}");
                assert!(error.contains("LLM_SMOLL_API_KEY"), "mode {mode}: {error}");
                assert!(error.contains("Set"), "mode {mode}: {error}");
            }
        }
    }
}

/// Malformed/invalid payload: a blank Smoll model or key is fatal, with status
/// 1, exactly one [ERROR] line, the canonical variable and configuration
/// guidance.
#[test]
fn when_starting_given_blank_smoll_model_or_key_then_exits_with_one_guided_error() {
    for mode in ["mid", "giga", "hybrid"] {
        for (missing, other) in [
            ("LLM_SMOLL_MODEL", ("LLM_SMOLL_API_KEY", "sk-smoll")),
            ("LLM_SMOLL_API_KEY", ("LLM_SMOLL_MODEL", SMOLL_MODEL)),
        ] {
            for blank in ["", "   ", "\t \n"] {
                let mut envs = mode_requirements(mode);
                envs.push(other);
                envs.push((missing, blank));

                let run = start(&envs);
                let error = assert_one_error(&run);
                assert!(error.contains(missing), "mode {mode}: {error}");
                assert!(
                    error.contains("LLM_SMOLL_MODEL") && error.contains("LLM_SMOLL_API_KEY"),
                    "mode {mode}: {error}"
                );
                assert!(error.contains("Set"), "mode {mode}: {error}");
            }
        }
    }
}

/// Minimum valid boundary: the required Smoll coordinates plus each mode's
/// existing provider requirements start the binary, with the default Messages
/// endpoint when no URL is supplied and a supplied override when there is one.
#[test]
fn when_starting_given_smoll_coordinates_and_mode_requirements_then_starts_with_the_default_endpoint()
 {
    assert_eq!(
        SMOLL_DEFAULT_URL, "https://api.anthropic.com/v1/messages",
        "the documented Messages endpoint"
    );

    for mode in ["mid", "giga", "hybrid"] {
        let mut default_envs = mode_requirements(mode);
        default_envs.extend(smoll_environment());
        let default = start(&default_envs);
        default.assert_success();
        default.assert_reports(&format!("summary -> anthropic-messages ({SMOLL_MODEL})"));

        for blank in ["", "   ", "\t \n"] {
            let mut envs = mode_requirements(mode);
            envs.extend(smoll_environment());
            envs.push(("LLM_SMOLL_URL", blank));
            let blank_url = start(&envs);
            blank_url.assert_success();
            blank_url.assert_reports(&format!("summary -> anthropic-messages ({SMOLL_MODEL})"));
        }

        let mut override_envs = mode_requirements(mode);
        override_envs.extend(smoll_environment());
        override_envs.push(("LLM_SMOLL_URL", SMOLL_URL));
        let overridden = start(&override_envs);
        overridden.assert_success();
        overridden.assert_reports(&format!("summary -> anthropic-messages ({SMOLL_MODEL})"));
    }
}

/// Unauthorized/forbidden boundary: every Mid and Giga credential, the old
/// generic and architect variables, and every auth switch cannot satisfy a
/// missing Smoll model or key. Startup refuses the fallback instead of routing
/// Summary elsewhere.
#[test]
fn when_configuring_given_other_provider_credentials_without_smoll_credentials_then_refuses_provider_fallback()
 {
    for mode in ["mid", "giga", "hybrid"] {
        let mut envs = full_environment(mode);
        envs.extend([
            (
                "LLM_PROVIDER_URL",
                "https://legacy.test/v1/chat/completions",
            ),
            ("LLM_MODEL", "legacy-model"),
            ("LLM_API_KEY", "legacy-key"),
            ("LLM_AUTH", "none"),
            ("LLM_ARCHITECT_URL", "https://legacy.test/v1/responses"),
            ("LLM_ARCHITECT_MODEL", "legacy-giga-model"),
            ("LLM_ARCHITECT_API_KEY", "legacy-key"),
            ("LLM_ARCHITECT_AUTH", "none"),
        ]);

        let run = start(&envs);
        let error = assert_one_error(&run);
        assert!(
            error.contains("LLM_SMOLL_MODEL") || error.contains("LLM_SMOLL_API_KEY"),
            "mode {mode}: {error}"
        );
        assert!(!error.contains(MID_MODEL), "mode {mode}: {error}");
        assert!(!error.contains(GIGA_MODEL), "mode {mode}: {error}");
    }
}
