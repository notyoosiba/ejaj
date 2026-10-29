//! Caller-intent test-title contract for the mid/giga rename.
//!
//! Every test introduced or materially rewritten by the rename must be named for
//! caller intent in domain language -- `when_<action>_given_<context>_then_<outcome>`
//! -- and must never carry an internal function or class name. These tests read
//! the checked-in source at the filesystem boundary, so a regression to an
//! implementation-shaped title fails loudly.

use std::path::PathBuf;

/// An internal function or class name that must not leak into a test title.
const INTERNAL_IDENTIFIERS: &[&str] = &[
    "mode_from_value",
    "mode_from_raw",
    "missing_giga_variable",
    "giga_requirement",
    "chat_provider",
    "responses_provider",
    "mid_from_values",
    "giga_from_values",
    "mid_from_env",
    "giga_from_env",
    "thinking_label",
    "group_digits",
    "provider_role",
    "summarize",
    "anthropic_provider",
    "native_tools",
    "tool_call_from_block",
    "nested_error_message",
    "call_time",
    "measured",
];

/// Rust builder method names that must not leak into an explicit-configuration
/// test title: the rewritten tests describe caller intent instead.
const CONFIGURATION_METHOD_IDENTIFIERS: &[&str] = &["new", "with_giga"];

/// One checked-in source file, read from the package root so the test works
/// from any working directory Cargo chooses.
fn source(relative: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(relative);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("could not read {}: {error}", path.display()))
}

/// The name of every function declared under a `#[test]` attribute.
fn test_titles(source: &str) -> Vec<String> {
    let mut titles = Vec::new();
    let mut pending = false;
    for line in source.lines() {
        let trimmed = line.trim();
        if trimmed == "#[test]" {
            pending = true;
            continue;
        }
        if !pending {
            continue;
        }
        if trimmed.is_empty() || trimmed.starts_with("//") || trimmed.starts_with("#[") {
            continue;
        }
        pending = false;
        if let Some(rest) = trimmed.strip_prefix("fn ")
            && let Some(name) = rest.split('(').next()
        {
            titles.push(name.trim().to_string());
        }
    }
    titles
}

/// The titles declared between two section markers in one source file.
fn titles_between(source: &str, start: &str, end: &str) -> Vec<String> {
    let rest = source
        .split_once(start)
        .unwrap_or_else(|| panic!("missing source marker `{start}`"))
        .1;
    let section = rest
        .split_once(end)
        .unwrap_or_else(|| panic!("missing source marker `{end}`"))
        .0;
    test_titles(section)
}

/// Assert the caller-intent `when_<action>_given_<context>_then_<outcome>` form
/// with every segment present.
fn assert_domain_level_bdd(title: &str) {
    assert!(
        title.starts_with("when_"),
        "test title must start with `when_`, not an internal name: {title}"
    );
    assert!(
        title
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
        "test title must be lowercase snake case: {title}"
    );
    let rest = title.strip_prefix("when_").unwrap();
    let (action, rest) = rest
        .split_once("_given_")
        .unwrap_or_else(|| panic!("test title has no `_given_`: {title}"));
    let (context, outcome) = rest
        .split_once("_then_")
        .unwrap_or_else(|| panic!("test title has no `_then_`: {title}"));
    assert!(
        !action.is_empty(),
        "test title has an empty action: {title}"
    );
    assert!(
        !context.is_empty(),
        "test title has an empty context: {title}"
    );
    assert!(
        !outcome.is_empty(),
        "test title has an empty outcome: {title}"
    );
}

/// Assert no internal function or class name leaked into a title.
fn assert_no_internal_identifier(title: &str) {
    for identifier in INTERNAL_IDENTIFIERS {
        assert!(
            !title.contains(identifier),
            "test title names the internal `{identifier}`: {title}"
        );
    }
}

/// Core entry-point behavior for the rewritten configuration tests: every title
/// in the provider-factory suite names the domain action, context and outcome,
/// and none names an internal function or class.
#[test]
fn when_renaming_given_rewritten_configuration_tests_then_uses_domain_level_titles() {
    let titles = test_titles(&source("src/llm/factory.rs"));
    assert!(!titles.is_empty(), "no configuration tests were found");
    for title in &titles {
        assert_domain_level_bdd(title);
        assert_no_internal_identifier(title);
    }
}

/// Regression for the provider-role label tests: the whole in-flight-status
/// section obeys the caller-intent contract, including its zero, grouping and
/// maximum-width cases.
#[test]
fn when_renaming_given_rewritten_provider_label_tests_then_uses_domain_level_titles() {
    let titles = titles_between(
        &source("src/status.rs"),
        "// -- Provider-role in-flight status",
        "// -- Log severity rendering",
    );
    assert!(!titles.is_empty(), "no provider-label tests were found");
    for title in &titles {
        assert_domain_level_bdd(title);
        assert_no_internal_identifier(title);
    }
}

/// Regression for the summary routing test: the one title that proves each mode
/// sends the summary pass to the provider its phase resolves to names the domain
/// action, context and outcome rather than the internal pass or phase types.
#[test]
fn when_renaming_given_rewritten_summary_routing_test_then_uses_domain_level_titles() {
    let titles = test_titles(&source("src/loops/summary.rs"));
    let routing: Vec<&String> = titles
        .iter()
        .filter(|title| title.contains("provider") && title.contains("summary"))
        .collect();
    assert_eq!(
        routing.len(),
        1,
        "expected exactly one summary routing test, found {routing:?}"
    );
    assert_domain_level_bdd(routing[0]);
    assert_no_internal_identifier(routing[0]);
}

/// Regression for the explicit-configuration tests: every configuration title
/// names the domain action, context and outcome, and none names a builder
/// method -- keyed, keyless and explicit-giga settings are described by what
/// the caller configured, not by the Rust API used to configure it.
#[test]
fn when_renaming_given_configuration_tests_then_uses_domain_level_titles() {
    let titles = test_titles(&source("src/config.rs"));
    let configuration: Vec<&String> = titles
        .iter()
        .filter(|title| title.starts_with("when_configuring_"))
        .collect();
    assert!(
        !configuration.is_empty(),
        "no configuration tests were found"
    );
    for title in configuration {
        assert_domain_level_bdd(title);
        assert_no_internal_identifier(title);
        for identifier in CONFIGURATION_METHOD_IDENTIFIERS {
            assert!(
                !title.contains(identifier),
                "test title names the internal `{identifier}`: {title}"
            );
        }
    }
}

/// Core entry-point contract for the native Messages provider: every title in
/// its suite names the domain action, context and outcome, and none names an
/// internal function or class.
#[test]
fn when_renaming_given_native_messages_provider_tests_then_uses_domain_level_titles() {
    let titles = test_titles(&source("src/llm/anthropic.rs"));
    assert!(
        !titles.is_empty(),
        "no native messages provider tests were found"
    );
    for title in &titles {
        assert_domain_level_bdd(title);
        assert_no_internal_identifier(title);
    }
}

/// Regression for the inline-duration timing suite: every title describes the
/// caller-visible operation in domain language, never the measurement seam.
#[test]
fn when_describing_timing_behavior_given_operation_tests_then_uses_domain_level_titles() {
    let titles = test_titles(&source("src/timing.rs"));
    assert!(!titles.is_empty(), "no timing tests were found");
    for title in &titles {
        assert_domain_level_bdd(title);
        assert_no_internal_identifier(title);
    }
    for expected in [
        "when_operation_finishes_given_success_then_preserves_result_and_elapsed_duration_without_reporting",
        "when_operation_fails_given_error_then_preserves_error_and_elapsed_duration_without_reporting",
        "when_operation_returns_nothing_given_absent_value_then_preserves_absence_and_elapsed_duration_without_reporting",
    ] {
        assert!(
            titles.iter().any(|title| title == expected),
            "missing caller-intent timing test `{expected}`: {titles:?}"
        );
    }
}

/// Regression for the command-duration formatter: its fixed-duration cases are
/// named for the command duration a caller sees, never for the private helper.
#[test]
fn when_describing_command_duration_given_fixed_durations_then_uses_domain_level_titles() {
    let titles = titles_between(
        &source("src/status.rs"),
        "// -- Tool-call rendering",
        "// -- Output coordinator",
    );
    let formatting: Vec<&String> = titles
        .iter()
        .filter(|title| title.contains("duration") && title.contains("format"))
        .collect();
    assert!(
        !formatting.is_empty(),
        "no duration-formatting tests were found"
    );
    for title in formatting {
        assert_domain_level_bdd(title);
        assert_no_internal_identifier(title);
    }
    assert!(
        titles.iter().any(|title| title
            == "when_command_duration_is_formatted_given_boundary_durations_then_floors_milliseconds"),
        "missing caller-intent boundary-duration test: {titles:?}"
    );
}

/// Regression for the output-coordinator rewrite: the materially rewritten log,
/// stream-transition and diagnostic coverage names the caller-visible behaviour
/// in domain language rather than the coordinator's internal machinery.
#[test]
fn when_renaming_given_rewritten_output_coordinator_tests_then_uses_domain_level_titles() {
    let titles = titles_between(
        &source("src/status.rs"),
        "// -- Output coordinator",
        "// -- Tool-execution timing",
    );
    for expected in [
        "when_logs_are_written_given_events_across_streams_then_preserve_adjacency_and_order",
        "when_logs_change_streams_given_non_log_output_then_keep_flush_order_and_spacing",
        "when_stderr_is_redirected_given_terminal_capabilities_then_preserve_stream_specific_rendering",
        "when_diagnostics_are_rendered_given_severity_and_unrecognized_tags_then_preserve_color_rules",
        "when_diagnostics_follow_logs_given_terminal_streams_then_keep_plain_payload_and_terminator",
    ] {
        let title = titles
            .iter()
            .find(|title| title.as_str() == expected)
            .unwrap_or_else(|| {
                panic!("missing caller-intent output-coordinator test `{expected}`: {titles:?}")
            });
        assert_domain_level_bdd(title);
        assert_no_internal_identifier(title);
    }
}

/// Regression for the summary body-line boundary coverage: the at-limit and
/// over-limit behaviours each carry their own caller-intent title, so the
/// combined title that named two outcomes cannot come back.
#[test]
fn when_renaming_given_summary_body_line_boundary_tests_then_uses_domain_level_titles() {
    let titles = test_titles(&source("tests/pipeline.rs"));
    let boundary: Vec<&String> = titles
        .iter()
        .filter(|title| title.starts_with("when_summarizing_given_body_line_"))
        .collect();
    assert_eq!(
        boundary.len(),
        2,
        "each body-line boundary behaviour needs its own caller-intent title: {boundary:?}"
    );
    for title in &boundary {
        assert_eq!(
            title.matches("_then_").count(),
            1,
            "a boundary title carries exactly one outcome: {title}"
        );
    }
    for expected in [
        "when_summarizing_given_body_line_at_limit_then_accepts",
        "when_summarizing_given_body_line_over_limit_then_rejects",
    ] {
        let title = titles
            .iter()
            .find(|title| title.as_str() == expected)
            .unwrap_or_else(|| {
                panic!("missing caller-intent boundary test `{expected}`: {titles:?}")
            });
        assert_domain_level_bdd(title);
        assert_no_internal_identifier(title);
    }
}
