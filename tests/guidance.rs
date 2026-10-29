//! Repository-guidance contract tests: the operator documentation must describe
//! the same canonical mid/giga configuration and hybrid routing the binary
//! implements. These tests read the checked-in files at the filesystem
//! boundary, so a stale mode table, variable block or startup rule fails loudly
//! before it can mislead an operator.

use std::fs;
use std::path::PathBuf;

/// One checked-in guidance file, read from the package root so the test works
/// from any working directory Cargo chooses.
fn guidance(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(name);
    fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("could not read {}: {error}", path.display()))
}

fn env_example() -> String {
    guidance(".env.example")
}

fn agents() -> String {
    guidance("AGENTS.md")
}

/// Empty/zero/nil boundary: with no mid overrides, the example still shows the
/// unchanged local LM Studio endpoint and model, plus the keyless auth switch.
#[test]
fn when_reading_guidance_given_unset_mid_overrides_then_shows_local_defaults() {
    let text = env_example();

    assert!(
        text.contains("LLM_MID_URL=http://192.168.56.1:1234/v1/chat/completions"),
        "the example no longer shows the unchanged mid local endpoint:\n{text}"
    );
    assert!(
        text.contains("LLM_MID_MODEL=local-model"),
        "the example no longer shows the unchanged mid local model:\n{text}"
    );
    assert!(
        text.contains("LLM_MID_AUTH=none"),
        "the example does not show keyless local mid use:\n{text}"
    );
}

/// Minimum valid boundary: giga needs both coordinates, and both guidance files
/// say so rather than describing a half-written block as usable.
#[test]
fn when_documenting_given_minimal_giga_configuration_then_requires_both_coordinates() {
    for (name, text) in [(".env.example", env_example()), ("AGENTS.md", agents())] {
        assert!(
            text.contains("LLM_GIGA_URL"),
            "{name} does not name LLM_GIGA_URL:\n{text}"
        );
        assert!(
            text.contains("LLM_GIGA_MODEL"),
            "{name} does not name LLM_GIGA_MODEL:\n{text}"
        );
        assert!(
            text.to_ascii_lowercase().contains("both"),
            "{name} does not say both giga coordinates are required:\n{text}"
        );
    }
}

/// Upper boundary: the worked example lists every canonical variable and the
/// full effort range, maximum included.
#[test]
fn when_reading_guidance_given_full_provider_example_then_documents_max_effort() {
    let text = env_example();

    for variable in [
        "LLM_MID_URL",
        "LLM_MID_MODEL",
        "LLM_MID_API_KEY",
        "LLM_MID_AUTH",
        "LLM_MID_REASONING_EFFORT",
        "LLM_GIGA_URL",
        "LLM_GIGA_MODEL",
        "LLM_GIGA_API_KEY",
        "LLM_GIGA_AUTH",
        "LLM_GIGA_REASONING_EFFORT",
    ] {
        assert!(
            text.contains(variable),
            "the example omits the canonical variable {variable}:\n{text}"
        );
    }
    assert!(
        text.contains("max"),
        "the example no longer documents the maximum effort:\n{text}"
    );
}

/// Malformed/invalid payload: a giga block carrying only one coordinate is a
/// startup failure, and the guidance says so with the canonical names.
#[test]
fn when_reading_guidance_given_partial_giga_configuration_then_documents_startup_failure() {
    let text = agents();
    let lower = text.to_ascii_lowercase();

    assert!(
        text.contains("LLM_GIGA_URL") && text.contains("LLM_GIGA_MODEL"),
        "the startup rules do not name both giga coordinates:\n{text}"
    );
    assert!(
        lower.contains("half-written") || lower.contains("half written"),
        "the startup rules do not describe a half-written giga block:\n{text}"
    );
    assert!(
        lower.contains("fatal"),
        "the startup rules do not call the failure fatal:\n{text}"
    );
    assert!(
        lower.contains("exit status 1") || lower.contains("exit 1") || lower.contains("status 1"),
        "the startup rules do not name the failure status:\n{text}"
    );
}

/// Unauthorized/forbidden boundary: a keyless https endpoint needs the
/// role-specific auth switch, and the guidance names mid and giga separately.
#[test]
fn when_reading_guidance_given_keyless_https_provider_then_documents_role_specific_auth() {
    let text = agents();

    assert!(
        text.to_ascii_lowercase().contains("https://"),
        "the auth rule no longer talks about https endpoints:\n{text}"
    );
    assert!(
        text.contains("LLM_MID_API_KEY") && text.contains("LLM_MID_AUTH=none"),
        "the mid auth rule is missing:\n{text}"
    );
    assert!(
        text.contains("LLM_GIGA_API_KEY") && text.contains("LLM_GIGA_AUTH=none"),
        "the giga auth rule is missing:\n{text}"
    );
    assert!(
        !text.contains("LLM_PROVIDER_URL"),
        "the guidance still names the legacy provider block:\n{text}"
    );
    assert!(
        !text.contains("LLM_ARCHITECT_URL"),
        "the guidance still names the legacy architect block:\n{text}"
    );
}

/// Core entry-point, contract and regression guardrail for the crate-wide
/// Clippy policy: the checked-in threshold must stay exactly 15 and the package
/// lint table must carry exactly the six complexity, nesting and boolean
/// assignments. The threshold must be the file's single root-level integer, any
/// other non-comment setting anywhere in the file is rejected, and the lint
/// table is isolated from its next TOML table boundary so matching text
/// elsewhere in the manifest cannot satisfy the contract. The empty/zero,
/// minimum-valid, quoted, wrong-table, extra-setting, wrong-name, wrong-level,
/// duplicate and repeated-section cases are rejected before the policy can
/// silently weaken.
#[test]
fn when_reading_guidance_given_clippy_configuration_then_enforces_complexity_and_nesting_lints() {
    // The one accepted root setting of a Clippy configuration file: blank
    // lines, whole-line comments, table headers and trailing comments carry no
    // setting, so a commented-out or absent key cannot satisfy the contract.
    // Only keys above the first table header are root-level, so a threshold
    // nested under any other table is not the crate-wide Clippy budget, and a
    // setting placed inside any following table is an extra setting the policy
    // does not declare, so the whole file is rejected. Values keep their TOML
    // representation, so a quoted string never passes for an integer.
    let root_settings = |text: &str| -> Option<Vec<(String, String)>> {
        let mut settings: Vec<(String, String)> = Vec::new();
        let mut at_root = true;
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or_default().trim();
            if line.is_empty() {
                continue;
            }
            if line.starts_with('[') {
                at_root = false;
                continue;
            }
            let (key, value) = line.split_once('=').unwrap_or_else(|| {
                panic!(
                    "TOML setting `{line}` has no `=`:
{text}"
                )
            });
            if !at_root {
                return None;
            }
            settings.push((key.trim().to_string(), value.trim().to_string()));
        }
        Some(settings)
    };

    // The effective assignments of exactly one `[lints.clippy]` section,
    // isolated from the next TOML table boundary or the end of the file. A
    // missing or repeated lint table is not the single policy section, and an
    // assignment placed in any other table is not part of it. Values keep their
    // TOML representation so an unquoted level cannot pass.
    let lint_assignments = |manifest: &str| -> Option<Vec<(String, String)>> {
        let mut assignments: Vec<(String, String)> = Vec::new();
        let mut lint_tables = 0usize;
        let mut in_lint_table = false;
        for line in manifest.lines() {
            let line = line.split('#').next().unwrap_or_default().trim();
            if line.is_empty() {
                continue;
            }
            if line.starts_with('[') {
                if line == "[lints.clippy]" {
                    lint_tables += 1;
                    if lint_tables > 1 {
                        return None;
                    }
                }
                in_lint_table = line == "[lints.clippy]";
                continue;
            }
            if !in_lint_table {
                continue;
            }
            let (key, value) = line.split_once('=').unwrap_or_else(|| {
                panic!(
                    "`[lints.clippy]` entry `{line}` has no `=`:
{manifest}"
                )
            });
            assignments.push((key.trim().to_string(), value.trim().to_string()));
        }
        (lint_tables == 1).then_some(assignments)
    };

    // The one accepted threshold and the six accepted lint assignments; every
    // other name, level or extra setting must be rejected.
    let threshold = vec![(
        "cognitive-complexity-threshold".to_string(),
        "15".to_string(),
    )];
    let mut lints: Vec<(String, String)> = [
        ("cognitive_complexity", "\"deny\""),
        ("collapsible_if", "\"deny\""),
        ("collapsible_match", "\"deny\""),
        ("collapsible_else_if", "\"deny\""),
        ("match_bool", "\"deny\""),
        ("manual_let_else", "\"warn\""),
    ]
    .into_iter()
    .map(|(lint, level)| (lint.to_string(), level.to_string()))
    .collect();
    lints.sort();

    let enforces_policy = |clippy_configuration: &str, manifest: &str| -> bool {
        let Some(mut configured) = lint_assignments(manifest) else {
            return false;
        };
        configured.sort();
        root_settings(clippy_configuration).as_deref() == Some(threshold.as_slice())
            && configured == lints
    };

    let valid_manifest = "[lints.clippy]
cognitive_complexity = \"deny\"
collapsible_if = \"deny\"
collapsible_match = \"deny\"
collapsible_else_if = \"deny\"
match_bool = \"deny\"
manual_let_else = \"warn\"
";
    let valid_threshold = "cognitive-complexity-threshold = 15";

    let cases: Vec<(&str, String, String, bool)> = vec![
        // Empty/zero/nil boundary: no threshold and no lint table at all.
        (
            "absent threshold and absent lints",
            String::new(),
            String::new(),
            false,
        ),
        // Empty/zero/nil boundary: a zero threshold is not the configured one.
        (
            "zero threshold",
            "cognitive-complexity-threshold = 0".to_string(),
            valid_manifest.to_string(),
            false,
        ),
        // Upper boundary: any value other than 15 is rejected.
        (
            "threshold of thirty",
            "cognitive-complexity-threshold = 30".to_string(),
            valid_manifest.to_string(),
            false,
        ),
        // A commented-out threshold is not a setting.
        (
            "commented-out threshold",
            "# cognitive-complexity-threshold = 15".to_string(),
            valid_manifest.to_string(),
            false,
        ),
        // A quoted threshold is a TOML string, not the root-level integer
        // budget the crate is configured with.
        (
            "quoted threshold",
            "cognitive-complexity-threshold = \"15\"".to_string(),
            valid_manifest.to_string(),
            false,
        ),
        // A non-integer numeric threshold is not the configured integer.
        (
            "floating-point threshold",
            "cognitive-complexity-threshold = 15.0".to_string(),
            valid_manifest.to_string(),
            false,
        ),
        // Placing the same threshold under an unrelated table must not satisfy
        // the root-level crate policy.
        (
            "threshold under an unrelated table",
            "[tool.clippy]
cognitive-complexity-threshold = 15"
                .to_string(),
            valid_manifest.to_string(),
            false,
        ),
        // A second setting beside the threshold widens the policy.
        (
            "extra clippy setting beside the threshold",
            format!("{valid_threshold}\ndisallowed-methods = [\"std::process::exit\"]"),
            valid_manifest.to_string(),
            false,
        ),
        // Empty/zero/nil boundary: comments and blank lines below a table header
        // declare no setting, so the single root threshold still stands alone.
        (
            "threshold followed by a comment-only table",
            format!("{valid_threshold}\n\n[tool.notes]\n# nothing configured here\n"),
            valid_manifest.to_string(),
            true,
        ),
        // An unrelated setting inside a following table is still an extra
        // non-comment setting, so the configuration must be rejected even
        // though the root threshold itself is exactly right.
        (
            "threshold followed by a table containing a setting",
            format!("{valid_threshold}\n[tool.audit]\nallow-unwrap-in-tests = true"),
            valid_manifest.to_string(),
            false,
        ),
        // The same extra setting in the first of several tables is rejected
        // whether or not more tables follow it.
        (
            "threshold followed by a setting in the first of several tables",
            format!("{valid_threshold}\n[disallowed]\nvalue = 1\n[tool.audit]\nvalue = 2"),
            valid_manifest.to_string(),
            false,
        ),
        // A setting table before the acceptable threshold moves the threshold
        // out of the file's root scope, so neither line is the crate budget.
        (
            "table containing a setting before the threshold",
            format!("[tool.audit]\nallow-unwrap-in-tests = true\n{valid_threshold}"),
            valid_manifest.to_string(),
            false,
        ),
        // Extra root-level settings of any name widen the policy.
        (
            "unrelated root-level setting above the threshold",
            format!("allow-unwrap-in-tests = true\n{valid_threshold}"),
            valid_manifest.to_string(),
            false,
        ),
        // Minimum valid boundary: exactly the configured policy.
        (
            "exact configured policy",
            valid_threshold.to_string(),
            valid_manifest.to_string(),
            true,
        ),
        // Contract boundary: the next table ends the lint table, so unrelated
        // assignments after it are not part of the policy.
        (
            "exact policy followed by another table",
            valid_threshold.to_string(),
            format!("{valid_manifest}[dependencies]\nserde_json = \"1.0\"\n"),
            true,
        ),
        // Wrong table placement: the same assignments under another lint scope.
        (
            "lints placed under another lint scope",
            valid_threshold.to_string(),
            valid_manifest.replace("[lints.clippy]", "[lints.rust]"),
            false,
        ),
        // Missing required assignment.
        (
            "missing nesting assignment",
            valid_threshold.to_string(),
            valid_manifest.replace("collapsible_else_if = \"deny\"\n", ""),
            false,
        ),
        // Lint levels are TOML strings: dropping the quotes is not the
        // specified representation of the required level.
        (
            "unquoted lint level",
            valid_threshold.to_string(),
            valid_manifest.replace("collapsible_if = \"deny\"", "collapsible_if = deny"),
            false,
        ),
        // Misspelled lint name, including Clippy's non-existent alias.
        (
            "misspelled boolean lint",
            valid_threshold.to_string(),
            valid_manifest.replace("match_bool", "match_nested_boolean"),
            false,
        ),
        // Wrong lint level for a required assignment.
        (
            "weakened manual let-else level",
            valid_threshold.to_string(),
            valid_manifest.replace("manual_let_else = \"warn\"", "manual_let_else = \"deny\""),
            false,
        ),
        // Unexpected assignment inside the scoped lint table.
        (
            "unexpected assignment in the lint table",
            valid_threshold.to_string(),
            format!("{valid_manifest}print_stdout = \"deny\"\n"),
            false,
        ),
        // Duplicate assignment inside the scoped lint table.
        (
            "duplicate assignment in the lint table",
            valid_threshold.to_string(),
            format!("{valid_manifest}collapsible_if = \"deny\"\n"),
            false,
        ),
        // An unrelated table before the lint table does not disturb the scope.
        (
            "unrelated table before the lint table",
            valid_threshold.to_string(),
            format!("[package]\nname = \"ejaj\"\n{valid_manifest}"),
            true,
        ),
        // A lint table emptied by its next boundary is not the same policy:
        // every required assignment must survive inside the one scoped table.
        (
            "lint table closed before its assignments",
            valid_threshold.to_string(),
            valid_manifest.replacen("[lints.clippy]\n", "[lints.clippy]\n[package]\n", 1),
            false,
        ),
        // Assignments split across repeated lint tables must not be aggregated
        // into a policy that no single section declares.
        (
            "assignments split across repeated lint tables",
            valid_threshold.to_string(),
            "[lints.clippy]
cognitive_complexity = \"deny\"
collapsible_if = \"deny\"
collapsible_match = \"deny\"
[lints.clippy]
collapsible_else_if = \"deny\"
match_bool = \"deny\"
manual_let_else = \"warn\"
"
            .to_string(),
            false,
        ),
        // A complete policy followed by another lint table is not exactly the
        // single section the contract verifies.
        (
            "complete policy followed by a repeated lint table",
            valid_threshold.to_string(),
            format!("{valid_manifest}[lints.clippy]\n"),
            false,
        ),
    ];

    let misclassified: Vec<&str> = cases
        .iter()
        .filter(|(_, clippy_configuration, manifest, accepted)| {
            enforces_policy(clippy_configuration, manifest) != *accepted
        })
        .map(|(label, ..)| *label)
        .collect();
    assert!(
        misclassified.is_empty(),
        "the Clippy policy check misclassifies these cases: {misclassified:?}"
    );

    let clippy_configuration = guidance(".clippy.toml");
    let manifest = guidance("Cargo.toml");
    assert!(
        enforces_policy(&clippy_configuration, &manifest),
        "the checked-in Clippy policy must set `cognitive-complexity-threshold = 15` and exactly the six required `[lints.clippy]` assignments:\n{clippy_configuration}\n{manifest}"
    );
}

fn crap_root_settings(text: &str) -> Option<Vec<(String, String)>> {
    let mut settings: Vec<(String, String)> = Vec::new();
    let mut at_root = true;
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or_default().trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            at_root = false;
            continue;
        }
        let (key, value) = line.split_once('=')?;
        if !at_root {
            return None;
        }
        settings.push((key.trim().to_string(), value.trim().to_string()));
    }
    Some(settings)
}

fn enforces_crap_config(crap_config: &str) -> bool {
    let Some(mut settings) = crap_root_settings(crap_config) else {
        return false;
    };
    settings.sort();
    settings
        == vec![
            ("fail-above".to_string(), "true".to_string()),
            ("threshold".to_string(), "30.0".to_string()),
        ]
}

fn active_workflow_lines(workflow: &str) -> String {
    workflow
        .lines()
        .map(|line| line.split('#').next().unwrap_or_default().trim())
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn contains_all(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().all(|needle| haystack.contains(needle))
}

fn appears_in_order(haystack: &str, steps: &[&str]) -> bool {
    let mut offset = 0usize;
    for step in steps {
        let Some(found) = haystack[offset..].find(step) else {
            return false;
        };
        offset += found + step.len();
    }
    true
}

fn enforces_crap_ci_policy(crap_config: &str, workflow: &str) -> bool {
    let active = active_workflow_lines(workflow);
    let required_tokens = [
        "push:",
        "pull_request:",
        "runs-on: ubuntu-latest",
        "stable",
        "clippy",
        "llvm-tools-preview",
        "cargo-llvm-cov",
        "cargo-crap",
    ];
    let ordered_commands = [
        "run: cargo clippy --all-targets -- -D warnings",
        "run: cargo llvm-cov --lcov --output-path lcov.info",
        "run: cargo crap --lcov lcov.info",
    ];
    enforces_crap_config(crap_config)
        && contains_all(&active, &required_tokens)
        && appears_in_order(&active, &ordered_commands)
}

/// Core entry-point, contract and regression guardrail for the coverage-based
/// CRAP policy and GitHub Actions workflow: `.cargo-crap.toml` must declare
/// only the root-level `threshold = 30.0` and `fail-above = true` settings, and
/// `.github/workflows/ci.yml` must run on `push` and `pull_request`, install
/// stable Rust with `clippy` and `llvm-tools-preview`, install
/// `cargo-llvm-cov` and `cargo-crap`, and execute the Clippy, LCOV generation
/// and `cargo crap --lcov lcov.info` gates in order.
#[test]
fn when_reading_guidance_given_crap_ci_configuration_then_enforces_coverage_gate() {
    let valid_crap = "threshold = 30.0\nfail-above = true\n";
    let valid_workflow = "\
name: CI
on:
  push:
  pull_request:
jobs:
  ci:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: clippy, llvm-tools-preview
      - run: cargo install cargo-llvm-cov cargo-crap --locked
      - run: cargo clippy --all-targets -- -D warnings
      - run: cargo llvm-cov --lcov --output-path lcov.info
      - run: cargo crap --lcov lcov.info
";

    let cases: Vec<(&str, String, String, bool)> = vec![
        // Empty/zero/nil boundary: absent configuration and absent workflow.
        ("empty config and workflow", String::new(), String::new(), false),
        // Empty/zero/nil boundary: zero threshold is rejected.
        (
            "zero threshold",
            "threshold = 0.0\nfail-above = true\n".to_string(),
            valid_workflow.to_string(),
            false,
        ),
        // Minimum valid boundary: exact two root settings and required CI steps.
        (
            "exact configured crap policy and workflow",
            valid_crap.to_string(),
            valid_workflow.to_string(),
            true,
        ),
        // Upper boundary: threshold above 30.0 is rejected.
        (
            "threshold above thirty",
            "threshold = 30.1\nfail-above = true\n".to_string(),
            valid_workflow.to_string(),
            false,
        ),
        // Malformed/invalid: quoted threshold or boolean strings are rejected.
        (
            "quoted threshold",
            "threshold = \"30.0\"\nfail-above = true\n".to_string(),
            valid_workflow.to_string(),
            false,
        ),
        (
            "quoted fail-above",
            "threshold = 30.0\nfail-above = \"true\"\n".to_string(),
            valid_workflow.to_string(),
            false,
        ),
        // Weakened or missing fail-above policy is rejected.
        (
            "disabled fail-above",
            "threshold = 30.0\nfail-above = false\n".to_string(),
            valid_workflow.to_string(),
            false,
        ),
        (
            "missing fail-above",
            "threshold = 30.0\n".to_string(),
            valid_workflow.to_string(),
            false,
        ),
        // Nested or extra TOML settings are rejected.
        (
            "settings nested under a table",
            "[crap]\nthreshold = 30.0\nfail-above = true\n".to_string(),
            valid_workflow.to_string(),
            false,
        ),
        (
            "extra root setting in crap config",
            format!("{valid_crap}missing = \"optimistic\"\n"),
            valid_workflow.to_string(),
            false,
        ),
        // Missing triggers, components, tools, LCOV path, or CRAP command fail.
        (
            "missing pull_request trigger",
            valid_crap.to_string(),
            valid_workflow.replace("  pull_request:\n", ""),
            false,
        ),
        (
            "missing llvm-tools-preview component",
            valid_crap.to_string(),
            valid_workflow.replace("llvm-tools-preview", "rustfmt"),
            false,
        ),
        (
            "missing cargo-llvm-cov install",
            valid_crap.to_string(),
            valid_workflow.replace("cargo-llvm-cov", "cargo-tarpaulin"),
            false,
        ),
        (
            "mismatched lcov output path",
            valid_crap.to_string(),
            valid_workflow.replace("--output-path lcov.info", "--output-path coverage.lcov"),
            false,
        ),
        (
            "commented-out cargo crap step",
            valid_crap.to_string(),
            valid_workflow.replace(
                "      - run: cargo crap --lcov lcov.info",
                "      # - run: cargo crap --lcov lcov.info",
            ),
            false,
        ),
        (
            "crap step ordered before lcov generation",
            valid_crap.to_string(),
            valid_workflow
                .replace(
                    "      - run: cargo llvm-cov --lcov --output-path lcov.info\n      - run: cargo crap --lcov lcov.info",
                    "      - run: cargo crap --lcov lcov.info\n      - run: cargo llvm-cov --lcov --output-path lcov.info",
                ),
            false,
        ),
    ];

    let misclassified: Vec<&str> = cases
        .iter()
        .filter(|(_, crap_config, workflow, accepted)| {
            enforces_crap_ci_policy(crap_config, workflow) != *accepted
        })
        .map(|(label, ..)| *label)
        .collect();
    assert!(
        misclassified.is_empty(),
        "the CRAP CI policy check misclassifies these cases: {misclassified:?}"
    );

    let crap_config = guidance(".cargo-crap.toml");
    let workflow = guidance(".github/workflows/ci.yml");
    assert!(
        enforces_crap_ci_policy(&crap_config, &workflow),
        "the checked-in `.cargo-crap.toml` and `.github/workflows/ci.yml` must enforce the coverage-based CRAP gate:\n{crap_config}\n{workflow}"
    );
}
