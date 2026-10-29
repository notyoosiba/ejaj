//! A repository guard: no tracked file may carry a model key.
//!
//! The real keys live in the untracked `.env`, which is the one place they
//! belong. This test keeps that true for the files git actually tracks -- the
//! example file, the docs, the source -- by scanning every tracked file for a
//! key-shaped token. A key is the `s`/`k` dash prefix followed by a long run of
//! token characters; a placeholder such as one ending in `REPLACE_ME` is
//! deliberately too short to match, so an example file stays legal.
//!
//! The prefix is assembled at runtime so this file does not contain the very
//! token it forbids. A match is reported redacted: the token itself is never
//! printed.

use std::path::Path;
use std::process::Command;

/// The shortest body that counts as a key rather than a placeholder. Real keys
/// carry far more; the examples in the tree carry far less.
const MIN_KEY_BODY: usize = 24;

/// Whether `c` may appear in a key body.
fn is_key_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// The first key-shaped token in `text`, described but never revealed.
fn key_shaped(text: &str) -> Option<String> {
    // Assembled, not written, so this source file carries no key prefix.
    let prefix = format!("{}{}", "s", "k-");
    let mut from = 0;
    while let Some(offset) = text[from..].find(&prefix) {
        let start = from + offset;
        let body_len = text[start + prefix.len()..]
            .chars()
            .take_while(|c| is_key_char(*c))
            .count();
        if body_len >= MIN_KEY_BODY {
            return Some(format!("{prefix}<redacted>"));
        }
        from = start + prefix.len();
    }
    None
}

/// Every file git tracks, or `None` when this is not a git checkout (a packaged
/// crate has no `.env` to leak and no repository to guard).
fn tracked_files() -> Option<Vec<String>> {
    let output = Command::new("git")
        .args(["ls-files", "-z"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&output.stdout)
            .split('\0')
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .collect(),
    )
}

/// No tracked file may contain a key-shaped token.
#[test]
fn no_tracked_file_carries_a_key() {
    let Some(files) = tracked_files() else {
        return;
    };
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));

    for file in files {
        let Ok(bytes) = std::fs::read(root.join(&file)) else {
            continue;
        };
        // A binary file cannot carry a key as text.
        let Ok(text) = std::str::from_utf8(&bytes) else {
            continue;
        };
        if let Some(description) = key_shaped(text) {
            panic!(
                "tracked file `{file}` contains a key-shaped token ({description}); real keys \
                 belong only in the untracked `.env`, never in a committed file"
            );
        }
    }
}

/// The `.env` file that holds the real keys must stay ignored.
#[test]
fn the_env_file_is_ignored() {
    let status = Command::new("git")
        .args(["check-ignore", "--no-index", "-q", ".env"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .status();
    let Ok(status) = status else {
        return;
    };
    assert!(
        status.success(),
        "`.env` is not gitignored; the real keys would be committed"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A run of real-key length is caught, and the report does not print it.
    #[test]
    fn a_long_token_is_detected_and_redacted() {
        let secret = format!("{}{}", "s", "k-0123456789abcdefghijklmnop");
        let report = key_shaped(&secret).expect("a long token is key-shaped");
        assert!(
            !report.contains("0123456789"),
            "the report leaked the token: {report}"
        );
    }

    /// A short placeholder is not a key.
    #[test]
    fn a_short_placeholder_is_not_a_key() {
        let placeholder = format!("{}{}", "s", "k-REPLACE_ME");
        assert_eq!(key_shaped(&placeholder), None);
    }

    /// A token just over the threshold is caught; one just under is not.
    #[test]
    fn the_threshold_is_the_body_length() {
        let under = format!(
            "{}{}",
            "s",
            "k-".to_string() + &"a".repeat(MIN_KEY_BODY - 1)
        );
        let over = format!("{}{}", "s", "k-".to_string() + &"a".repeat(MIN_KEY_BODY));
        assert_eq!(key_shaped(&under), None);
        assert!(key_shaped(&over).is_some());
    }
}
