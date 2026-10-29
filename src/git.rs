//! Git integration: the state a project root must be in before a run starts.
//!
//! One invocation is one run against one project root, and the run is anchored
//! to the root it was constructed with rather than to wherever the process
//! happens to be. Before the plan agent is asked anything, that root's working
//! tree must be clean: a run that started over someone else's uncommitted work
//! could not tell the changes it makes from the ones that were already there.
//!
//! Exactly one result counts as clean: `git status --porcelain
//! --untracked-files=all` exits successfully and prints nothing. A change of
//! any visible class -- staged, unstaged, or untracked and not ignored --
//! refuses the run. An ignored-only change, the pipeline's own artifact
//! directory under `.ejaj/` included, is not a change at all and is accepted.
//! A status that could not be produced -- Git missing, the root not a
//! repository, the command exiting unsuccessfully -- is a failure too: an
//! unreadable status is never treated as an empty, clean one.
//!
//! The same module owns the two operations that record a finished run: stage
//! every visible change at the project root ('git add --all -- . :(exclude).ejaj'),
//! then create one ordinary commit whose message is handed to Git as input data
//! rather than interpolated into a shell ('git commit --cleanup=verbatim -F -',
//! the canonical summary written to the child's stdin). The default-branch
//! workflow saves the same visible changes with the same exclusion before it
//! switches branches. Every one of those operations is rooted at the retained
//! project path, all preserve Git's own diagnostics, and a failure names the
//! operation that failed.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// The Git pathspec that excludes the pipeline's own artifact directory and
/// all of its descendants. It is a negative pathspec, so it neither needs nor
/// depends on a root .gitignore rule: tracked and otherwise unignored .ejaj
/// paths are excluded just as untracked ones are. The same exclusion is applied
/// to the status read, the default-branch stash, and final staging, keeping the
/// run's artifacts outside the changes the run itself records.
const RUN_ARTIFACT_PATHSPEC: &str = ":(exclude).ejaj";

/// The artifact exclusion used when the repository's own ignore rules hide the
/// '.ejaj/' directory.
///
/// Git's staging walk reports the literal ':(exclude).ejaj' as an ignored path
/// to add when the directory it names is hidden by a root ignore rule, and
/// exits non-zero before recording anything. Spelling the leading dot as the
/// equivalent glob '[\.]' keeps the same root-relative exclusion while giving
/// the pathspec the wildcard Git needs to stop treating it as an ignored add
/// target; the trailing '/**' subtracts the whole artifact tree. The exclusion
/// still removes tracked and otherwise unignored artifacts, so it is the
/// pathspec, not the ignore rule, that keeps them out.
const IGNORED_RUN_ARTIFACT_PATHSPEC: &str = ":(exclude,glob)[.]ejaj/**";

/// The status read every preflight uses. The explicit `--porcelain` and
/// `--untracked-files=all` keep the result machine-readable and complete, so no
/// end user's `status.short`, `status.showUntrackedFiles` or `color.status`
/// setting can hide a change or dress the output up. The whole project root is
/// the positive pathspec, and the artifact pathspec subtracts .ejaj and its
/// descendants from it: the pair keeps every ordinary staged, unstaged and
/// untracked change visible while the run's handoffs never gate its next
/// invocation, even when the artifacts are tracked or not ignored.
const STATUS_ARGS: [&str; 6] = [
    "status",
    "--porcelain",
    "--untracked-files=all",
    "--",
    WHOLE_TREE_PATHSPEC,
    RUN_ARTIFACT_PATHSPEC,
];

/// The branch currently checked out. A strict symbolic reference read: a
/// detached HEAD has no branch identity, so it is a failure rather than an
/// empty or guessed name. The full reference is read -- not the short form --
/// so a target outside `refs/heads/` can never be mistaken for a branch.
const CURRENT_BRANCH_ARGS: [&str; 3] = ["symbolic-ref", "--quiet", "HEAD"];

/// The local remote-tracking marker whose absent/present state decides whether
/// a finished run records locally or uses the default-branch workflow.
const ORIGIN_HEAD_REFERENCE: &str = "refs/remotes/origin/HEAD";

/// The full symbolic target of the local remote HEAD, e.g.
/// `refs/remotes/origin/main`. The full reference is read -- not the shortened
/// form -- so a target outside `refs/remotes/origin/` can never be mistaken
/// for the remote default branch just because Git can shorten it to
/// `origin/<branch>`.
const DEFAULT_BRANCH_ARGS: [&str; 3] = ["symbolic-ref", "--quiet", ORIGIN_HEAD_REFERENCE];

/// Construct one Git command rooted at `root`, with terminal prompting disabled.
///
/// Every non-commit Git operation in this module goes through this one helper so
/// no subprocess can inherit the terminal and block on a credential prompt:
/// stdin is always the null device, and `GIT_TERMINAL_PROMPT` is forced to `0`
/// even when the parent environment sets it to something else. The commit
/// operation is the sole exception -- it builds its own command because it feeds
/// the summary to Git over a piped stdin -- and overrides the same variable there.
fn git_command(root: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .current_dir(root)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null());
    command
}

/// Refuse a project root the run may not start from.
///
/// `Ok(())` means the root's working tree is clean: the status read succeeded
/// and reported nothing. Every other result is the module's one failure shape --
/// a [`GitFailure`] carrying the `"preflight"` stage and a human-readable
/// diagnostic the caller reports: the visible changes that must be committed,
/// stashed, discarded or ignored, or the reason the status could not be read at
/// all.
pub(crate) fn preflight(root: &Path) -> Result<(), GitFailure> {
    let status = status(root).map_err(preflight_failure)?;
    if status.trim().is_empty() {
        return Ok(());
    }

    Err(preflight_failure(format!(
        "the project root {} has uncommitted changes; a clean working tree is required before the \
         agents start. Commit, stash or discard them -- or ignore files that are not part of the \
         project -- and start again. `git status --porcelain --untracked-files=all` reports:\n{}",
        root.display(),
        status.trim_end_matches('\n'),
    )))
}

/// A failed preflight, in the same staged shape as the operations that record a
/// finished run: the status read that begins every invocation is an operation
/// too, and the caller reports it exactly like the staging and the commit.
fn preflight_failure(reason: String) -> GitFailure {
    GitFailure {
        stage: "preflight",
        reason,
    }
}

/// `git status --porcelain --untracked-files=all` in `root`, as its stdout.
///
/// The read itself can fail -- Git cannot be spawned, the root does not exist,
/// or the root is not a repository -- and each of those is reported as its own
/// diagnostic rather than folded into an empty, clean-looking result.
fn status(root: &Path) -> Result<String, String> {
    let output = git_command(root)
        .args(STATUS_ARGS)
        .output()
        .map_err(|error| {
            format!(
                "could not run `git status --porcelain --untracked-files=all` in {}: {error}. A \
                 readable status is required before the agents start: without it there is no way \
                 to tell a clean working tree from one that cannot be read.",
                root.display(),
            )
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        return Err(format!(
            "`git status --porcelain --untracked-files=all` failed in {} ({}): {}. A readable \
             status is required before the agents start: an unreadable status is never treated \
             as a clean working tree.",
            root.display(),
            output.status,
            if stderr.is_empty() {
                "no diagnostic on stderr".to_string()
            } else {
                stderr.to_string()
            },
        ));
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// A Git operation a run needed and could not complete.
///
/// One shape covers every fatal Git failure -- the preflight that starts an
/// invocation, every operation in the default-branch sequence, and the staging
/// and commit that record a finished run -- so a caller never has to tell one
/// variant from another, only which operation failed and why. No failure rolls
/// the repository back: the commits, refs, stash entries, index and worktree
/// stay exactly where the failed operation left them.
pub(crate) struct GitFailure {
    /// Where the run failed: "preflight" for the status read every invocation
    /// begins with; "branch-detection" for the current/default branch identity
    /// read; "branch" for the summary-branch uniqueness check and the branch
    /// creation; "stash" for saving the run's changes (and verifying the new
    /// entry); "switch" for switching to the default branch; "pull" for the
    /// fast-forward-only update; "stash-pop" for reapplying the saved changes;
    /// "stage" for 'git add --all'; "commit" for the commit that follows it; and
    /// "push" for publishing the summary branch.
    pub(crate) stage: &'static str,
    /// The diagnostic: the exit status and whatever Git wrote to stderr, or the
    /// reason the process could not even be spawned.
    pub(crate) reason: String,
}

/// The positive pathspec every artifact exclusion is paired with.
///
/// Git honours a negative pathspec only alongside at least one positive one, so
/// the whole project root is offered first and the run's own artifact directory
/// is then subtracted from it. The root is the project path every Git command
/// is rooted at, so '.' names exactly the working tree the run may record.
const WHOLE_TREE_PATHSPEC: &str = ".";

/// The artifact directory the run owns at the project root.
const ARTIFACT_DIRECTORY: &str = ".ejaj";

/// The pathspecs a recording Git operation applies to the changes at 'root'.
///
/// The whole project root is always the positive pathspec, and the required
/// negative pathspec excluding '.ejaj' is always subtracted from it. In the
/// ordinary case that is '-- . :(exclude).ejaj'. Git's staging machinery
/// refuses that literal negative pathspec when it also matches a directory the
/// repository's own ignore rules hide -- it names the directory and exits
/// non-zero before anything is recorded -- and repositories that already
/// gitignore '.ejaj/' are exactly that case. There the same exclusion is
/// spelled with the equivalent glob form in
/// IGNORED_RUN_ARTIFACT_PATHSPEC, so the whole tree is still offered to Git and
/// the artifacts are still subtracted from it. Either form only bounds what the
/// recording command itself adds or stashes; callers first clear artifact
/// entries staged before the run through unstage_artifact_changes.
fn recorded_change_pathspecs(root: &Path, stage: &'static str) -> Result<Vec<String>, GitFailure> {
    if artifact_directory_is_ignored(root) {
        if visible_top_level_pathspecs(root, stage)?.is_empty() {
            return Ok(Vec::new());
        }
        return Ok(vec![
            WHOLE_TREE_PATHSPEC.to_string(),
            IGNORED_RUN_ARTIFACT_PATHSPEC.to_string(),
        ]);
    }
    Ok(vec![
        WHOLE_TREE_PATHSPEC.to_string(),
        RUN_ARTIFACT_PATHSPEC.to_string(),
    ])
}

/// Whether the repository's own ignore rules hide the artifact directory.
///
/// One real Git query over the exclude rules themselves: '--no-index' keeps a
/// directory that also carries tracked artifacts from being treated as tracked
/// and therefore "not ignored", which would otherwise select the negative
/// pathspec Git rejects for an ignored directory. Anything other than a
/// confirmed match counts as "not ignored", so the negative pathspec stays the
/// default and a root Git cannot answer for is reported by the recording
/// command itself.
fn artifact_directory_is_ignored(root: &Path) -> bool {
    git_command(root)
        .args([
            "check-ignore",
            "--quiet",
            "--no-index",
            "--",
            ARTIFACT_DIRECTORY,
        ])
        .status()
        .is_ok_and(|status| status.success())
}

/// The positive top-level pathspecs that cover every visible change at 'root'.
///
/// 'git ls-files --cached --others --exclude-standard' is the one listing that
/// holds both tracked entries and untracked, not-ignored ones, so the pathspecs
/// built from it cover every addition, modification and deletion Git can
/// record. Each visible path is reduced to its literal top-level component and
/// the artifact directory is dropped. The ignored-directory recording route
/// uses the empty result as the signal that there is no visible change to save
/// or stage -- the ordinary route leaves that judgement to Git itself -- and
/// otherwise records the whole tree with the artifact exclusion.
fn visible_top_level_pathspecs(
    root: &Path,
    stage: &'static str,
) -> Result<Vec<String>, GitFailure> {
    let output = git_command(root)
        .args([
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
        ])
        .output()
        .map_err(|error| GitFailure {
            stage,
            reason: format!(
                "could not run 'git ls-files' in {}: {error}. Every visible change outside the \
                 run's artifacts must be identifiable before the run records them.",
                root.display(),
            ),
        })?;

    if !output.status.success() {
        return Err(GitFailure {
            stage,
            reason: format!(
                "'git ls-files' failed in {} ({}): {}",
                root.display(),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim_end(),
            ),
        });
    }

    let mut components = BTreeSet::new();
    for path in output.stdout.split(|byte| *byte == 0) {
        let path = String::from_utf8_lossy(path);
        if let Some(component) = path.split('/').next()
            && !component.is_empty()
            && component != ARTIFACT_DIRECTORY
        {
            components.insert(component.to_string());
        }
    }

    Ok(components
        .into_iter()
        .map(|component| format!(":(literal){component}"))
        .collect())
}

/// Clear artifact changes a caller staged before the run from the index,
/// without touching a single artifact byte on disk.
///
/// A recording pathspec only bounds what one Git command adds or stashes; it
/// cannot remove entries that were already in the index when the run started.
/// Resetting just the artifact directory to HEAD drops staged additions,
/// modifications and deletions there while every working-tree artifact stays
/// exactly as it was, so neither the stash nor the commit can record them. An
/// unborn HEAD has nothing to reset to, so the artifact entries are removed
/// from the index directly -- the only index state that can exist before the
/// first commit.
fn unstage_artifact_changes(root: &Path, stage: &'static str) -> Result<(), GitFailure> {
    if head_exists(root, stage)? {
        return run_git(
            root,
            &["reset", "--quiet", "HEAD", "--", ARTIFACT_DIRECTORY],
            stage,
            "clear previously staged artifact changes from the index",
        )
        .map(|_| ());
    }

    run_git(
        root,
        &[
            "rm",
            "--cached",
            "--quiet",
            "--recursive",
            "--ignore-unmatch",
            "--",
            ARTIFACT_DIRECTORY,
        ],
        stage,
        "clear previously staged artifact changes from the index",
    )
    .map(|_| ())
}

/// Whether HEAD names a commit, so the artifact index can be reset to it.
///
/// An unborn branch is the one state with no commit to reset to; every other
/// unreadable HEAD is fatal rather than being mistaken for an empty history.
fn head_exists(root: &Path, stage: &'static str) -> Result<bool, GitFailure> {
    let output = git_command(root)
        .args(["rev-parse", "--verify", "--quiet", "HEAD"])
        .output()
        .map_err(|error| GitFailure {
            stage,
            reason: format!(
                "could not run 'git rev-parse --verify --quiet HEAD' in {} before clearing staged \
                 artifact changes: {error}. The run's artifacts must be identifiable in the index \
                 before the run records its changes.",
                root.display(),
            ),
        })?;

    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(GitFailure {
            stage,
            reason: format!(
                "'git rev-parse --verify --quiet HEAD' failed in {} ({}) before clearing staged \
                 artifact changes: {}. The run's artifacts must be identifiable in the index \
                 before the run records its changes.",
                root.display(),
                output.status,
                stderr(&output.stderr),
            ),
        }),
    }
}

/// The branch currently checked out at 'root'.
///
/// Only a real symbolic branch counts: a detached HEAD has no branch identity,
/// a target outside `refs/heads/` is not a local branch, and either is reported
/// as a branch-detection-stage failure rather than guessed at. The identity is
/// read exactly as Git printed it -- only the line terminator is removed -- and
/// an empty name or one that carries whitespace or control characters is
/// malformed and refused, never trimmed into a valid-looking branch.
pub(crate) fn current_branch(root: &Path) -> Result<String, GitFailure> {
    let reference = read_branch(root, &CURRENT_BRANCH_ARGS, "HEAD", "the current branch")?;
    let branch = reference
        .strip_prefix("refs/heads/")
        .ok_or_else(|| GitFailure {
            stage: "branch-detection",
            reason: format!(
                "`git symbolic-ref --quiet HEAD` in {} resolved to `{reference}`, which is not \
                 a local branch under `refs/heads/`. A readable current branch is required \
                 before the run can be recorded.",
                root.display(),
            ),
        })?;
    if branch.is_empty() {
        return Err(GitFailure {
            stage: "branch-detection",
            reason: format!(
                "`git symbolic-ref --quiet HEAD` in {} resolved to `{reference}`, which names \
                 no local branch. A readable current branch is required before the run can be \
                 recorded.",
                root.display(),
            ),
        });
    }
    Ok(branch.to_string())
}

/// The default branch the local remote HEAD names at 'root', without the
/// `origin/` prefix.
///
/// `None` is the one non-fatal outcome: the local
/// `refs/remotes/origin/HEAD` marker is confirmed absent by every local query
/// and by the loose reference path itself. That absence lets the run record
/// locally on whichever readable branch is checked out.
///
/// Anything present is read strictly and in full: an unreadable, non-symbolic,
/// malformed or dangling `origin/HEAD`, and a target that does not begin
/// exactly with `refs/remotes/origin/` or carries no branch suffix, is a
/// branch-detection-stage failure. A shortened name is never trusted: Git can
/// shorten a target outside the remote-tracking namespace to text that looks
/// like `origin/<branch>`, so only the complete symbolic target decides the
/// identity. No network is touched -- the remote-tracking reference is local
/// state.
pub(crate) fn default_branch(root: &Path) -> Result<Option<String>, GitFailure> {
    if origin_head_is_absent(root)? {
        return Ok(None);
    }
    let reference = read_branch(
        root,
        &DEFAULT_BRANCH_ARGS,
        ORIGIN_HEAD_REFERENCE,
        "a readable origin/HEAD",
    )?;
    let branch = reference
        .strip_prefix("refs/remotes/origin/")
        .ok_or_else(|| GitFailure {
            stage: "branch-detection",
            reason: format!(
                "`git symbolic-ref --quiet refs/remotes/origin/HEAD` in {} resolved to \
             `{reference}`, which is not a full remote-tracking branch under \
             `refs/remotes/origin/`. A readable origin/HEAD naming the default branch is \
             required before the run can be recorded.",
                root.display(),
            ),
        })?;
    if branch.is_empty() {
        return Err(GitFailure {
            stage: "branch-detection",
            reason: format!(
                "`git symbolic-ref --quiet refs/remotes/origin/HEAD` in {} resolved to \
                 `{reference}`, which names no branch under `refs/remotes/origin/`. A readable \
                 origin/HEAD naming the default branch is required before the run can be \
                 recorded.",
                root.display(),
            ),
        });
    }
    verify_remote_tracking_branch(root, branch)?;
    Ok(Some(branch.to_string()))
}

/// Whether the local `refs/remotes/origin/HEAD` marker is confirmed absent.
///
/// A reference Git can resolve -- even one whose target is invalid -- is
/// present, and so is a loose reference path that exists but cannot be read as
/// a symbolic reference. Only a marker no local query can see is absent. Any
/// query that cannot be completed is a branch-detection failure: an unreadable
/// marker is never mistaken for an absent one.
fn origin_head_is_absent(root: &Path) -> Result<bool, GitFailure> {
    if origin_head_ref_exists(root)? {
        return Ok(false);
    }
    if origin_head_path_present(root)? {
        return Ok(false);
    }
    Ok(true)
}

/// Whether Git can resolve the `origin/HEAD` reference. A direct reference and
/// a symbolic reference whose target resolves both count as present; exit code
/// 1 means Git saw no such reference, and any other outcome is indeterminate
/// and fatal.
fn origin_head_ref_exists(root: &Path) -> Result<bool, GitFailure> {
    let output = git_command(root)
        .args(["show-ref", "--verify", "--quiet", ORIGIN_HEAD_REFERENCE])
        .output()
        .map_err(|error| GitFailure {
            stage: "branch-detection",
            reason: format!(
                "could not run 'git show-ref --verify --quiet {ORIGIN_HEAD_REFERENCE}' in {} \
                 while confirming whether origin/HEAD is absent: {error}. An indeterminate \
                 origin/HEAD state is fatal before the run can be recorded.",
                root.display(),
            ),
        })?;

    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(GitFailure {
            stage: "branch-detection",
            reason: format!(
                "'git show-ref --verify --quiet {ORIGIN_HEAD_REFERENCE}' failed in {} ({}) while \
                 confirming whether origin/HEAD is absent: {}. An indeterminate origin/HEAD \
                 state is fatal before the run can be recorded.",
                root.display(),
                output.status,
                stderr(&output.stderr),
            ),
        }),
    }
}

/// Whether the loose reference path Git names for `origin/HEAD` exists.
///
/// This catches a present-but-unreadable marker that resolves to no reference:
/// a directory or an unreadable file is still present, never absent. A path
/// that cannot even be resolved is indeterminate and fatal.
fn origin_head_path_present(root: &Path) -> Result<bool, GitFailure> {
    let output = git_command(root)
        .args([
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            ORIGIN_HEAD_REFERENCE,
        ])
        .output()
        .map_err(|error| GitFailure {
            stage: "branch-detection",
            reason: format!(
                "could not run 'git rev-parse --path-format=absolute --git-path \
                 {ORIGIN_HEAD_REFERENCE}' in {} while confirming whether origin/HEAD is absent: \
                 {error}. An indeterminate origin/HEAD state is fatal before the run can be \
                 recorded.",
                root.display(),
            ),
        })?;

    if !output.status.success() {
        return Err(GitFailure {
            stage: "branch-detection",
            reason: format!(
                "'git rev-parse --path-format=absolute --git-path {ORIGIN_HEAD_REFERENCE}' \
                 failed in {} ({}) while confirming whether origin/HEAD is absent: {}. An \
                 indeterminate origin/HEAD state is fatal before the run can be recorded.",
                root.display(),
                output.status,
                stderr(&output.stderr),
            ),
        });
    }

    let printed = String::from_utf8_lossy(&output.stdout);
    let path = strip_line_terminator(&printed);
    if path.is_empty() {
        return Err(GitFailure {
            stage: "branch-detection",
            reason: format!(
                "'git rev-parse --path-format=absolute --git-path {ORIGIN_HEAD_REFERENCE}' in {} \
                 printed an empty path. An indeterminate origin/HEAD state is fatal before the \
                 run can be recorded.",
                root.display(),
            ),
        });
    }

    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(GitFailure {
            stage: "branch-detection",
            reason: format!(
                "could not read the origin/HEAD reference path {path} in {} while confirming \
                 whether it is absent: {error}. An indeterminate origin/HEAD state is fatal \
                 before the run can be recorded.",
                root.display(),
            ),
        }),
    }
}

/// Require the remote-tracking reference `origin/HEAD` named to resolve.
///
/// A symbolic reference can survive the branch it names: `origin/HEAD` still
/// prints `origin/main` after `refs/remotes/origin/main` is gone. That is a
/// dangling identity, not a default branch, and it is fatal here -- before any
/// finalization mutation -- rather than later at the pull.
fn verify_remote_tracking_branch(root: &Path, branch: &str) -> Result<(), GitFailure> {
    let reference = format!("refs/remotes/origin/{branch}");
    let output = git_command(root)
        .args(["show-ref", "--verify", "--quiet", &reference])
        .output()
        .map_err(|error| GitFailure {
            stage: "branch-detection",
            reason: format!(
                "could not run 'git show-ref --verify --quiet {reference}' in {} while resolving \
                 origin/HEAD: {error}. A readable origin/HEAD target is required before the run \
                 can be recorded.",
                root.display(),
            ),
        })?;

    match output.status.code() {
        Some(0) => Ok(()),
        Some(1) => Err(GitFailure {
            stage: "branch-detection",
            reason: format!(
                "origin/HEAD in {} names `origin/{branch}`, but `{reference}` does not resolve \
                 to a reference. A dangling origin/HEAD is a branch-detection failure: no \
                 default branch can be named before the run can be recorded.",
                root.display(),
            ),
        }),
        _ => Err(GitFailure {
            stage: "branch-detection",
            reason: format!(
                "'git show-ref --verify --quiet {reference}' failed in {} ({}): {}. A readable \
                 origin/HEAD target is required before the run can be recorded.",
                root.display(),
                output.status,
                stderr(&output.stderr),
            ),
        }),
    }
}

/// The stdout of a command without the one line terminator it writes, and
/// nothing else.
///
/// A trailing `\r` is removed only as half of a `\r\n` terminator. A lone
/// carriage return, any other whitespace and every other byte belong to the
/// printed text, so removing them here would rewrite the identity the caller is
/// about to trust.
fn strip_line_terminator(stdout: &str) -> &str {
    match stdout.strip_suffix('\n') {
        Some(line) => line.strip_suffix('\r').unwrap_or(line),
        None => stdout,
    }
}

/// Read one symbolic branch reference and require a well-formed identity.
///
/// Only the printed line terminator is removed. The identity itself is never
/// trimmed: whitespace inside it is part of the reference, and stripping it
/// would normalize a malformed identity into an existing valid branch name the
/// caller would then act on. An empty identity, or one carrying whitespace or a
/// control character, is a branch-detection failure.
fn read_branch(
    root: &Path,
    args: &[&str],
    reference: &str,
    what: &str,
) -> Result<String, GitFailure> {
    let output = git_command(root)
        .args(args)
        .output()
        .map_err(|error| GitFailure {
            stage: "branch-detection",
            reason: format!(
                "could not run `git {}` in {} while reading {what}: {error}. A readable branch \
                 identity is required before the run can be recorded.",
                args.join(" "),
                root.display(),
            ),
        })?;

    if !output.status.success() {
        return Err(GitFailure {
            stage: "branch-detection",
            reason: format!(
                "`git {}` failed in {} ({}): {}. A readable {reference} naming the {what} is \
                 required before the run can be recorded.",
                args.join(" "),
                root.display(),
                output.status,
                stderr(&output.stderr),
            ),
        });
    }

    let printed = String::from_utf8_lossy(&output.stdout);
    let name = strip_line_terminator(&printed);
    if name.is_empty() {
        return Err(GitFailure {
            stage: "branch-detection",
            reason: format!(
                "`git {}` in {} printed an empty branch name. A readable {reference} naming the \
                 {what} is required before the run can be recorded.",
                args.join(" "),
                root.display(),
            ),
        });
    }
    if name
        .chars()
        .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err(GitFailure {
            stage: "branch-detection",
            reason: format!(
                "`git {}` in {} printed `{name}`, which is a malformed {what} identity: a \
                 branch reference cannot carry whitespace or control characters. A readable \
                 {reference} naming the {what} is required before the run can be recorded.",
                args.join(" "),
                root.display(),
            ),
        });
    }
    Ok(name.to_string())
}

/// Record a finished run on the branch that is checked out.
///
/// The current branch identity is read first, strictly: an unreadable,
/// malformed or non-branch current identity, and a detached HEAD, fail before
/// anything is staged, stashed, switched or committed. The default-branch
/// identity is read next; a present `origin/HEAD` that is unreadable,
/// non-symbolic, malformed, dangling or out-of-prefix is just as fatal. When
/// the marker is confirmed absent, the run is recorded locally on whichever
/// readable branch is checked out, default branch included: every non-ignored
/// change is staged with 'git add --all' and one commit whose message is
/// exactly 'message' is created, with no stash, switch, pull, branch creation,
/// push or remote operation. On a non-default branch under a valid marker the
/// same local recording runs. No stash, switch, pull, branch creation or push
/// runs, so the default branch and every remote ref stay exactly where they
/// were.
///
/// On the default branch under a valid marker the recording never advances that
/// branch. The run's
/// visible changes are saved with an untracked-including stash, the checked-out
/// branch is switched to the default branch, that branch is fast-forwarded from
/// 'origin <default>' explicitly, and only after the pull succeeds is a unique
/// summary branch created from the updated tip. The saved changes are
/// reapplied, staged, committed once on the new branch and pushed to 'origin'
/// with upstream tracking and a create-only lease on the exact destination ref.
/// Every operation is its own direct `Command` argument vector rooted at
/// `root`, and the first failure stops the sequence with an operation-specific
/// stage. Nothing is rolled back, force-updated or retried: a failed stash,
/// switch, pull, branch query/creation, stash-pop, stage, commit or push returns
/// at once, preserving the stash, the local branch and its commit, and the
/// remote state exactly as the failure left them.
pub(crate) fn finalize(root: &Path, message: &str) -> Result<(), GitFailure> {
    let current = current_branch(root)?;
    let default = default_branch(root)?;
    // A confirmed-absent marker records locally on the checked-out branch,
    // default branch included. Only a valid marker can select the
    // default-branch workflow, and a non-default branch under a valid marker
    // keeps the existing local route.
    if let Some(default) = default
        && current == default
    {
        return finalize_default(root, &default, message);
    }
    record_locally(root, message)
}

/// Stage the whole working tree and commit 'message' on the current branch.
fn record_locally(root: &Path, message: &str) -> Result<(), GitFailure> {
    stage_all(root)?;
    commit(root, message)
}

/// The maximum slug length a derived summary branch carries after `ejaj/`.
const SLUG_LIMIT: usize = 235;
/// The fixed prefix every default-branch summary branch carries.
const BRANCH_PREFIX: &str = "ejaj/";

/// Record a run that finished on the default branch.
///
/// The sequence follows the acceptance contract exactly: save visible changes,
/// switch to the default branch, fast-forward it from the named remote branch,
/// derive and check a unique branch, create it, optionally reapply the stash,
/// stage, commit once, and push with upstream tracking. Nothing is forced,
/// reused or rolled back: a collision or a racing remote branch is a fatal
/// failure at the operation that discovered it.
fn finalize_default(root: &Path, default: &str, message: &str) -> Result<(), GitFailure> {
    // A status read that cannot be produced is not an empty one: the run's
    // visible changes cannot be identified, so the sequence stops before
    // anything is saved or moved.
    let visible = status(root).map_err(|reason| GitFailure {
        stage: "stash",
        reason: format!(
            "could not read the working tree status before saving this run's changes in {}: \
             {reason}. The default-branch recording cannot start without knowing which changes \
             belong to the run.",
            root.display(),
        ),
    })?;
    // Only a non-empty status has visible work to save; an empty one skips
    // both the stash and the reapply. The new entry's object identity is
    // retained and is the only entry ever reapplied.
    let stash = if visible.trim().is_empty() {
        None
    } else {
        Some(stash_push(root)?)
    };

    switch(root, default)?;
    pull_default(root, default)?;

    let branch = summary_branch(message);
    ensure_branch_absent(root, &branch)?;
    create_branch(root, &branch)?;

    if let Some(stash) = &stash {
        stash_pop(root, stash)?;
    }

    stage_all(root)?;
    commit(root, message)?;
    push_branch(root, &branch)
}

/// Derive the summary branch name from the canonical summary's header.
///
/// The header is the first line of the canonical summary. Every ASCII letter
/// is lowercased and every ASCII digit is retained; each maximal run of every
/// other character -- punctuation, spaces, non-ASCII characters included --
/// becomes one hyphen. Leading and trailing hyphens are removed, the slug is
/// capped at 235 ASCII characters, any hyphen left at the truncation boundary
/// is trimmed, and the fixed `ejaj/` prefix is prepended, so the whole name is
/// at most 240 ASCII characters.
fn summary_branch(message: &str) -> String {
    let header = message.lines().next().unwrap_or("");
    let mut slug = String::new();
    let mut separator = false;
    for character in header.chars() {
        if character.is_ascii_alphanumeric() {
            if separator && !slug.is_empty() {
                slug.push('-');
            }
            separator = false;
            slug.push(character.to_ascii_lowercase());
        } else {
            separator = true;
        }
    }

    if slug.len() > SLUG_LIMIT {
        slug.truncate(SLUG_LIMIT);
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    format!("{BRANCH_PREFIX}{slug}")
}

/// Save every visible change, untracked and not-ignored files included, in one
/// invocation-specific stash and return that new entry's object identity.
///
/// Artifact entries staged before the run are cleared from the index first, so
/// their working-tree contents stay on disk but cannot travel into the saved
/// entry. The push is then rooted at the project and subtracts the artifact
/// pathspec: 'git stash push --include-untracked --quiet --message "ejaj run
/// changes" -- . :(exclude).ejaj', with the equivalent glob spelling of that
/// exclusion when the repository already gitignores '.ejaj/' and Git refuses
/// the literal negative pathspec beside the whole tree. The exclusion is what
/// keeps tracked and otherwise unignored .ejaj paths out of the saved entry,
/// no matter what the root ignore rules say.
///
/// The entries present before the push are read first. A push that reports
/// success without adding exactly one new entry is a stash failure, and so is a
/// push that leaves any visible change behind: the caller retains the returned
/// identity and reapplies exactly that entry, so a pre-existing stash is never
/// the one that is popped.
fn stash_push(root: &Path) -> Result<String, GitFailure> {
    let before = stash_entries(root).map_err(|reason| GitFailure {
        stage: "stash",
        reason: format!(
            "could not read the stash entries in {} before saving this run's changes: {reason}. \
             The run's own entry must be identifiable before the default branch is switched.",
            root.display(),
        ),
    })?;
    unstage_artifact_changes(root, "stash")?;
    let pathspecs = recorded_change_pathspecs(root, "stash")?;
    if pathspecs.is_empty() {
        return Err(GitFailure {
            stage: "stash",
            reason: format!(
                "no visible change outside .ejaj was found in {} to save before switching to \
                 the default branch. The run's changes must be identifiable before the default \
                 branch is switched, so the sequence stops here.",
                root.display(),
            ),
        });
    }
    let mut stash_args: Vec<String> = [
        "stash",
        "push",
        "--include-untracked",
        "--quiet",
        "--message",
        "ejaj run changes",
        "--",
    ]
    .iter()
    .map(|argument| argument.to_string())
    .collect();
    stash_args.extend(pathspecs);
    let stash_args: Vec<&str> = stash_args.iter().map(String::as_str).collect();
    run_git(
        root,
        &stash_args,
        "stash",
        "save the run's visible changes before switching to the default branch",
    )?;
    let after = stash_entries(root).map_err(|reason| GitFailure {
        stage: "stash",
        reason: format!(
            "could not read the stash entries in {} after saving this run's changes: {reason}. \
             The run's own entry must be identified before the default branch is switched.",
            root.display(),
        ),
    })?;
    if after.len() != before.len() + 1 {
        return Err(GitFailure {
            stage: "stash",
            reason: format!(
                "'git stash push --include-untracked --quiet --message ejaj run changes' in {} \
                 reported success but did not create exactly one new stash entry. The run's \
                 changes must be saved before the default branch is switched, so the sequence \
                 stops here.",
                root.display(),
            ),
        });
    }
    let stash = after
        .first()
        .expect("a push that added one entry leaves a non-empty list")
        .clone();

    // The saved entry is not enough: the visible worktree itself must be gone,
    // or switching branches would carry changes the run cannot account for.
    let remaining = status(root).map_err(|reason| GitFailure {
        stage: "stash",
        reason: format!(
            "could not read the working tree status in {} after saving this run's changes: \
             {reason}. The run's changes must be verified as saved before the default branch is \
             switched.",
            root.display(),
        ),
    })?;
    if !remaining.trim().is_empty() {
        return Err(GitFailure {
            stage: "stash",
            reason: format!(
                "'git stash push --include-untracked --quiet --message ejaj run changes' in {} \
                 reported success but visible changes remain in the working tree. The run's \
                 changes must be saved before the default branch is switched, so the sequence \
                 stops here. `git status --porcelain --untracked-files=all` reports:\n{}",
                root.display(),
                remaining.trim_end_matches('\n'),
            ),
        });
    }

    Ok(stash)
}

/// The stash entries, newest first, as the object id each reflog entry names.
///
/// A failed read is a diagnostic string rather than an empty list: an
/// unreadable stash is never mistaken for no stash at all.
fn stash_entries(root: &Path) -> Result<Vec<String>, String> {
    let output = git_command(root)
        .args(["stash", "list", "--format=%H"])
        .output()
        .map_err(|error| {
            format!(
                "could not run 'git stash list --format=%H' in {}: {error}",
                root.display()
            )
        })?;

    if !output.status.success() {
        return Err(format!(
            "'git stash list --format=%H' failed in {} ({}): {}",
            root.display(),
            output.status,
            stderr(&output.stderr),
        ));
    }

    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect())
}

/// Reapply the exact stash entry this invocation created, then drop it.
///
/// The retained object identity is resolved back to its current reflog
/// reference and that reference is popped: an entry another process inserted in
/// the meantime is never the one applied or dropped, and the pre-existing stash
/// entries stay where they were.
fn stash_pop(root: &Path, stash: &str) -> Result<(), GitFailure> {
    let entries = stash_entries(root).map_err(|reason| GitFailure {
        stage: "stash-pop",
        reason: format!(
            "could not read the stash entries in {} before reapplying this run's changes: \
             {reason}. The run's own entry must be found before it can be reapplied.",
            root.display(),
        ),
    })?;
    let index = entries
        .iter()
        .position(|entry| entry == stash)
        .ok_or_else(|| GitFailure {
            stage: "stash-pop",
            reason: format!(
                "the stash entry this run created (`{stash}`) is no longer in {}'s stash reflog. \
                 Only the run's own entry is ever reapplied, so the sequence stops here.",
                root.display(),
            ),
        })?;
    let reference = format!("stash@{{{index}}}");
    run_git(
        root,
        &["stash", "pop", "--quiet", &reference],
        "stash-pop",
        "reapply the run's changes on the new summary branch",
    )
    .map(|_| ())
}

/// Switch the working tree to the named default branch.
fn switch(root: &Path, branch: &str) -> Result<(), GitFailure> {
    run_git(
        root,
        &["switch", "--quiet", branch],
        "switch",
        "switch to the default branch before fast-forwarding it",
    )
    .map(|_| ())
}

/// Fast-forward the checked-out default branch from the explicit remote branch.
/// No merge and no rebase can turn a diverged default branch into a run branch.
fn pull_default(root: &Path, default: &str) -> Result<(), GitFailure> {
    run_git(
        root,
        &["pull", "--ff-only", "--quiet", "origin", default],
        "pull",
        "fast-forward the default branch from origin before branching",
    )
    .map(|_| ())
}

/// Create the new branch from the current, just-fast-forwarded tip.
fn create_branch(root: &Path, branch: &str) -> Result<(), GitFailure> {
    run_git(
        root,
        &["switch", "--quiet", "--create", branch],
        "branch",
        "create the unique summary branch after the default branch was updated",
    )
    .map(|_| ())
}

/// Push the new branch to `origin` and set its upstream.
///
/// The push is create-only: the lease names the exact destination ref with an
/// empty expected object, so the remote must not carry that ref at all. A ref
/// that appeared after the uniqueness check is therefore rejected even when it
/// is an ancestor of the run commit and an ordinary fast-forward would have
/// overwritten it. Nothing is forced into an existing ref and nothing is
/// reused.
fn push_branch(root: &Path, branch: &str) -> Result<(), GitFailure> {
    let destination = format!("refs/heads/{branch}");
    let refspec = format!("{branch}:{destination}");
    let lease = format!("--force-with-lease={destination}:");
    run_git(
        root,
        &[
            "push",
            "--quiet",
            "--set-upstream",
            &lease,
            "origin",
            &refspec,
        ],
        "push",
        "publish the summary branch with upstream tracking",
    )
    .map(|_| ())
}

/// Refuse a derived branch name that already exists locally or exactly on
/// `origin`. Both checks are strict: a local exit code of 1 means absent, and
/// any failure to query the remote is fatal rather than being read as absence.
fn ensure_branch_absent(root: &Path, branch: &str) -> Result<(), GitFailure> {
    let reference = format!("refs/heads/{branch}");
    let local = git_command(root)
        .args(["show-ref", "--verify", "--quiet", &reference])
        .output()
        .map_err(|error| GitFailure {
            stage: "branch",
            reason: format!(
                "could not run 'git show-ref --verify --quiet {reference}' in {} before creating \
                 the summary branch: {error}. A readable local ref check is required so an \
                 existing branch is never reused.",
                root.display(),
            ),
        })?;

    match local.status.code() {
        Some(0) => {
            return Err(GitFailure {
                stage: "branch",
                reason: format!(
                    "the summary branch '{branch}' already exists locally in {}. A derived branch \
                     name is never reused or overwritten; the run stops before creating it.",
                    root.display(),
                ),
            });
        }
        Some(1) => {}
        _ => {
            return Err(GitFailure {
                stage: "branch",
                reason: format!(
                    "'git show-ref --verify --quiet {reference}' failed in {} ({}): {}. A readable \
                     local ref check is required so an existing branch is never reused.",
                    root.display(),
                    local.status,
                    stderr(&local.stderr),
                ),
            });
        }
    }

    let remote = git_command(root)
        .args(["ls-remote", "--heads", "origin", &reference])
        .output()
        .map_err(|error| GitFailure {
            stage: "branch",
            reason: format!(
                "could not run 'git ls-remote --heads origin {reference}' in {} before creating \
                 the summary branch: {error}. A readable origin query is required so an existing \
                 remote branch is never reused.",
                root.display(),
            ),
        })?;

    if !remote.status.success() {
        return Err(GitFailure {
            stage: "branch",
            reason: format!(
                "'git ls-remote --heads origin {reference}' failed in {} ({}): {}. A failed \
                 origin query is fatal, never interpreted as the branch being absent.",
                root.display(),
                remote.status,
                stderr(&remote.stderr),
            ),
        });
    }

    if !String::from_utf8_lossy(&remote.stdout).trim().is_empty() {
        return Err(GitFailure {
            stage: "branch",
            reason: format!(
                "the summary branch '{branch}' already exists on origin. A derived branch name is \
                 never reused or overwritten; the run stops before creating it."
            ),
        });
    }

    Ok(())
}

/// Run one direct Git argument vector rooted at `root`, returning its stdout.
///
/// The stage names the operation that failed, so a caller reports the first
/// failing operation rather than folding every failure into one shape. Git's
/// own stderr is preserved, and a spawn failure is just as fatal.
fn run_git(
    root: &Path,
    args: &[&str],
    stage: &'static str,
    what: &str,
) -> Result<String, GitFailure> {
    let output = git_command(root)
        .args(args)
        .output()
        .map_err(|error| GitFailure {
            stage,
            reason: format!(
                "could not run 'git {}' in {} to {what}: {error}",
                args.join(" "),
                root.display(),
            ),
        })?;

    if !output.status.success() {
        return Err(GitFailure {
            stage,
            reason: format!(
                "'git {}' failed in {} ({}) while trying to {what}: {}",
                args.join(" "),
                root.display(),
                output.status,
                stderr(&output.stderr),
            ),
        });
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Stage every visible change outside the run's artifact directory at 'root':
/// 'git add --all -- . :(exclude).ejaj', or, when the repository already
/// gitignores '.ejaj/' and Git therefore refuses the negative pathspec beside
/// the whole tree, the positive top-level pathspecs plus the same exclusion;
/// see recorded_change_pathspecs.
///
/// Artifact entries staged before the run are cleared from the index first, so
/// their working-tree contents stay on disk but cannot reach the commit. The
/// whole working tree is then offered to Git, not a guessed list of source
/// files, so additions, modifications and deletions are all staged. The
/// required negative pathspec keeps the pipeline's own .ejaj/ paths out whether
/// they are ignored, tracked or otherwise unignored -- in the ignored-directory
/// case in its equivalent glob spelling, because Git refuses the literal one
/// there -- so no ignore rule has to be consulted to keep the run's artifacts
/// out of the commit. When nothing outside .ejaj is visible there is nothing to
/// stage and the commit stage reports the missing change. A failure is returned
/// as a GitFailure carrying the "stage" name and Git's own diagnostic.
pub(crate) fn stage_all(root: &Path) -> Result<(), GitFailure> {
    unstage_artifact_changes(root, "stage")?;
    let pathspecs = recorded_change_pathspecs(root, "stage")?;
    if pathspecs.is_empty() {
        return Ok(());
    }

    let output = git_command(root)
        .args(["add", "--all", "--"])
        .args(&pathspecs)
        .output()
        .map_err(|error| GitFailure {
            stage: "stage",
            reason: format!(
                "could not run 'git add --all' in {}: {error}. Every visible change outside the \
                 run's artifacts must be staged before the run can commit its summary.",
                root.display(),
            ),
        })?;

    if !output.status.success() {
        return Err(GitFailure {
            stage: "stage",
            reason: format!(
                "'git add --all' failed in {} ({}): {}",
                root.display(),
                output.status,
                stderr(&output.stderr),
            ),
        });
    }

    Ok(())
}

/// Create one ordinary commit at 'root' whose message is exactly 'message'.
///
/// The summary is supplied as input data -- written to the child's stdin by the
/// '-F -' argument, never interpolated into a shell command -- and
/// '--cleanup=verbatim' keeps comment-like lines and the summary's own
/// formatting exactly as they were written. Nothing is amended and no empty
/// commit is forced: a repository with nothing to commit fails, and that failure
/// is reported with the "commit" stage name rather than masked as success.
pub(crate) fn commit(root: &Path, message: &str) -> Result<(), GitFailure> {
    let mut child = Command::new("git")
        .args(["commit", "--cleanup=verbatim", "-F", "-"])
        .current_dir(root)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| GitFailure {
            stage: "commit",
            reason: format!(
                "could not run 'git commit --cleanup=verbatim -F -' in {}: {error}",
                root.display(),
            ),
        })?;

    // Write the canonical bytes to the child and close its stdin, whatever the
    // write's outcome: the child must still be waited for so its diagnostic is
    // not lost.
    let write = child
        .stdin
        .take()
        .map(|mut stdin| stdin.write_all(message.as_bytes()));
    let output = child.wait_with_output().map_err(|error| GitFailure {
        stage: "commit",
        reason: format!(
            "could not wait for 'git commit --cleanup=verbatim -F -' in {}: {error}",
            root.display(),
        ),
    })?;

    if let Some(Err(error)) = write {
        return Err(GitFailure {
            stage: "commit",
            reason: format!(
                "could not write the summary to 'git commit --cleanup=verbatim -F -' in {}: \
                 {error}. {}",
                root.display(),
                stderr(&output.stderr),
            ),
        });
    }

    if !output.status.success() {
        return Err(GitFailure {
            stage: "commit",
            reason: format!(
                "'git commit --cleanup=verbatim -F -' failed in {} ({}): {}",
                root.display(),
                output.status,
                stderr(&output.stderr),
            ),
        });
    }

    Ok(())
}

/// Git's stderr as a diagnostic, or a placeholder when it wrote none.
fn stderr(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        "no diagnostic on stderr".to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::path::PathBuf;

    #[test]
    fn when_running_git_given_a_project_root_then_commands_use_the_root_and_disable_terminal_prompts()
     {
        let root = PathBuf::from("chosen-project-root");
        let command = git_command(&root);

        assert_eq!(command.get_current_dir(), Some(root.as_path()));
        let prompt = command
            .get_envs()
            .find(|(name, _)| *name == OsStr::new("GIT_TERMINAL_PROMPT"))
            .and_then(|(_, value)| value);
        assert_eq!(prompt, Some(OsStr::new("0")));
    }
}
