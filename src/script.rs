//! In-process JavaScript execution engine (`execute_script`) backed by QuickJS.
//!
//! All `rquickjs` types stay inside this module. The rest of the crate talks to
//! [`ScriptEngine`] using [`Capabilities`] and plain Rust strings, keeping the
//! agent loop and transport synchronous and decoupled from the JS runtime.

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rquickjs::prelude::{Opt, Rest};
use rquickjs::{
    CatchResultExt, CaughtError, Context, Ctx, Error as JsError, Exception, Function, IntoJs,
    Object, Runtime, Value,
};

use crate::tools::Phase;

/// Maximum heap memory a single script execution may allocate (32 MiB).
pub const MEMORY_LIMIT_BYTES: usize = 32 * 1024 * 1024;

/// Maximum wall-clock time a single script execution may run (10 seconds).
pub const DEFAULT_DEADLINE: Duration = Duration::from_secs(10);

/// Maximum UTF-8 byte length of the output returned to the model (16 KiB).
pub const OUTPUT_LIMIT_BYTES: usize = 16 * 1024;

/// Maximum UTF-8 byte length of the JavaScript source code accepted (256 KiB).
pub const SOURCE_LIMIT_BYTES: usize = 256 * 1024;

/// Maximum number of match records `tools.grep` materializes and returns (1,000).
pub const GREP_MATCH_LIMIT: usize = 1_000;

/// Maximum UTF-8 bytes retained for one matched `tools.grep` line (2,000).
pub const GREP_LINE_MAX_BYTES: usize = 2_000;

/// Per-phase capability set and root directory confinement for [`ScriptEngine`].
///
/// `Phase::Code` receives all six bindings (`read`, `write`, `replace`, `list`,
/// `grep`, `diff`); every other phase (`Phase::Plan`, `Phase::Spec`,
/// `Phase::Tickets`, `Phase::Summary`) receives only the read-only bindings
/// (`read`, `list`, `grep`, `diff`), making their read-only contract true by
/// construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    /// The project root to which every filesystem and git operation is confined.
    pub root: PathBuf,
    /// Whether mutating filesystem bindings (`write`, `replace`) are exposed.
    pub writable: bool,
}

impl Capabilities {
    /// Capabilities for `phase`, rooted at the current working directory.
    pub fn for_phase(phase: Phase) -> Self {
        let root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self {
            root,
            writable: matches!(phase, Phase::Code),
        }
    }

    /// Read-only capabilities (`read`, `list`, `grep`) rooted at `root`.
    pub fn read_only(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            writable: false,
        }
    }

    /// Read-write capabilities (`read`, `write`, `replace`, `list`, `grep`)
    /// rooted at `root`.
    pub fn read_write(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            writable: true,
        }
    }

    /// Override the root directory while preserving the phase's write capability.
    pub fn with_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.root = root.into();
        self
    }
}

/// Synchronous facade over the script execution engine.
///
/// The crate talks to the engine through this trait in terms of
/// [`Capabilities`] and plain Rust strings; [`QuickJsEngine`] is the only
/// implementation and the only runtime the tools ever dispatch to.
pub trait ScriptEngine {
    /// Run `code` in a fresh sandbox configured with `caps`.
    fn run(&self, code: &str, caps: &Capabilities) -> Result<String, String>;
}

/// Ephemeral QuickJS engine: every [`ScriptEngine::run`] call creates a fresh
/// `Runtime` and `Context` so no global state survives across calls.
#[derive(Debug, Clone)]
pub struct QuickJsEngine {
    deadline: Duration,
    memory_limit: usize,
    output_limit: usize,
    source_limit: usize,
}

impl Default for QuickJsEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl QuickJsEngine {
    /// A QuickJS engine configured with the production resource limits.
    pub const fn new() -> Self {
        Self {
            deadline: DEFAULT_DEADLINE,
            memory_limit: MEMORY_LIMIT_BYTES,
            output_limit: OUTPUT_LIMIT_BYTES,
            source_limit: SOURCE_LIMIT_BYTES,
        }
    }

    /// Override the wall-clock deadline (used by unit tests to verify timeout
    /// enforcement without waiting the full 10 seconds).
    pub const fn with_deadline(mut self, deadline: Duration) -> Self {
        self.deadline = deadline;
        self
    }

    /// Override the memory limit in bytes.
    pub const fn with_memory_limit(mut self, memory_limit: usize) -> Self {
        self.memory_limit = memory_limit;
        self
    }

    /// Override the output cap in bytes.
    pub const fn with_output_limit(mut self, output_limit: usize) -> Self {
        self.output_limit = output_limit;
        self
    }

    /// Override the source cap in bytes.
    pub const fn with_source_limit(mut self, source_limit: usize) -> Self {
        self.source_limit = source_limit;
        self
    }
}

/// Ensure every script error is wrapped in the self-correcting `[ERROR: ...]`
/// shape expected by the model and the agent loop.
pub fn format_script_error(detail: impl AsRef<str>) -> String {
    let text = detail.as_ref();
    if text.starts_with("[ERROR:") {
        text.to_string()
    } else {
        format!("[ERROR: execute_script failed]\n{text}")
    }
}

/// Truncate script output at `limit` bytes on a UTF-8 character boundary,
/// appending a marker that names the limit, the exact number of source bytes
/// the model did not see, and the JavaScript-side filtering advice. The whole
/// returned value -- content and marker together -- is never longer than
/// `limit` bytes.
pub fn cap_script_output(text: String, limit: usize) -> String {
    if text.len() <= limit {
        return text;
    }

    let mut keep = limit.min(text.len());
    loop {
        let end = floor_char_boundary(&text, keep);
        let omitted = text.len() - end;
        let marker = format!(
            "\n[script output truncated to {limit} bytes; {omitted} bytes omitted; filter \
             results in JavaScript so the returned value stays small]"
        );
        if end + marker.len() <= limit {
            let mut output = String::with_capacity(end + marker.len());
            output.push_str(&text[..end]);
            output.push_str(&marker);
            return output;
        }

        if end == 0 {
            // The cap is too small even for the marker on its own. Stay bounded
            // and keep whatever content fits, rather than returning a value
            // over the cap.
            return text[..floor_char_boundary(&text, limit)].to_string();
        }

        let excess = (end + marker.len()).saturating_sub(limit);
        keep = end.saturating_sub(excess.max(1));
    }
}

/// The largest index at or below `index` that is a UTF-8 character boundary.
fn floor_char_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

impl ScriptEngine for QuickJsEngine {
    fn run(&self, code: &str, caps: &Capabilities) -> Result<String, String> {
        if code.len() > self.source_limit {
            return Err(format_script_error(format!(
                "script source is {} bytes, which exceeds the {}-byte source limit",
                code.len(),
                self.source_limit
            )));
        }

        let runtime = Runtime::new().map_err(|e| {
            format_script_error(format!("failed to initialize QuickJS runtime: {e}"))
        })?;
        runtime.set_memory_limit(self.memory_limit);

        let timed_out = Arc::new(AtomicBool::new(false));
        let timed_out_flag = Arc::clone(&timed_out);
        let deadline = Instant::now() + self.deadline;
        runtime.set_interrupt_handler(Some(Box::new(move || {
            if Instant::now() >= deadline {
                timed_out_flag.store(true, Ordering::Relaxed);
                true
            } else {
                false
            }
        })));

        let context = Context::full(&runtime).map_err(|e| {
            format_script_error(format!("failed to initialize QuickJS context: {e}"))
        })?;

        let logs: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

        let raw_output = context.with(|ctx| -> Result<String, String> {
            install_console(&ctx, Arc::clone(&logs))
                .map_err(|e| format_script_error(format!("failed to install console: {e}")))?;
            install_tools(&ctx, caps)
                .map_err(|e| format_script_error(format!("failed to install tools: {e}")))?;

            let value = evaluate_script(&ctx, code, &timed_out, self.deadline, self.memory_limit)?;

            if !value.is_undefined() {
                if let Some(s) = value.as_string() {
                    let rendered = s.to_string().map_err(|e| {
                        format_script_error(format!("failed to decode string output: {e}"))
                    })?;
                    return Ok(rendered);
                }
                match ctx.json_stringify(value).catch(&ctx) {
                    Ok(Some(json)) => {
                        let rendered = json.to_string().map_err(|e| {
                            format_script_error(format!("failed to decode JSON output: {e}"))
                        })?;
                        return Ok(rendered);
                    }
                    Ok(None) => {}
                    Err(caught) => {
                        return Err(format_caught_error(
                            caught,
                            &timed_out,
                            self.deadline,
                            self.memory_limit,
                        ));
                    }
                }
            }

            let captured = logs.lock().unwrap().join("\n");
            Ok(captured)
        })?;

        Ok(cap_script_output(raw_output, self.output_limit))
    }
}

/// Evaluate `code`. Plain script evaluation runs first so expression-terminated
/// scripts return their completion value; if parsing fails because the script
/// uses a top-level `return` statement, the source is evaluated inside a
/// function body instead. Because a `SyntaxError` occurs during parsing, no
/// side effects have run before the fallback.
fn evaluate_script<'js>(
    ctx: &Ctx<'js>,
    code: &str,
    timed_out: &AtomicBool,
    deadline: Duration,
    memory_limit: usize,
) -> Result<Value<'js>, String> {
    match ctx.eval::<Value<'js>, _>(code).catch(ctx) {
        Ok(value) => Ok(value),
        Err(caught) => {
            if is_top_level_return_syntax_error(&caught) {
                let wrapped = format!("(function() {{\n{code}\n}})()");
                return ctx
                    .eval::<Value<'js>, _>(wrapped)
                    .catch(ctx)
                    .map_err(|err| format_caught_error(err, timed_out, deadline, memory_limit));
            }
            Err(format_caught_error(
                caught,
                timed_out,
                deadline,
                memory_limit,
            ))
        }
    }
}

/// Whether `caught` is a parse-time `SyntaxError` caused by a top-level
/// `return` statement outside a function body.
fn is_top_level_return_syntax_error(caught: &CaughtError<'_>) -> bool {
    match caught {
        CaughtError::Exception(ex) => {
            let text = ex.to_string();
            text.contains("return not in a function")
        }
        _ => false,
    }
}

/// Convert a QuickJS [`CaughtError`] into a self-correcting `[ERROR: ...]`
/// message, distinguishing deadline interrupts and memory-limit breaches from
/// ordinary runtime exceptions.
fn format_caught_error(
    caught: CaughtError<'_>,
    timed_out: &AtomicBool,
    deadline: Duration,
    memory_limit: usize,
) -> String {
    if timed_out.load(Ordering::Relaxed) {
        return format_script_error(format!(
            "script exceeded the {:?} wall-clock deadline",
            deadline
        ));
    }

    match caught {
        CaughtError::Error(JsError::Allocation) => format_script_error(format!(
            "script exceeded the {memory_limit}-byte memory limit"
        )),
        CaughtError::Exception(ex) => {
            let text = ex.to_string();
            if text.contains("out of memory") {
                return format_script_error(format!(
                    "script exceeded the {memory_limit}-byte memory limit"
                ));
            }
            format_script_error(text)
        }
        CaughtError::Error(err) => format_script_error(err.to_string()),
        CaughtError::Value(val) => format_script_error(format!("uncaught value: {val:?}")),
    }
}

/// Install `console.log` (plus `info`, `warn`, `error`) backed by `logs`.
fn install_console<'js>(ctx: &Ctx<'js>, logs: Arc<Mutex<Vec<String>>>) -> rquickjs::Result<()> {
    let log_fn = Function::new(
        ctx.clone(),
        move |args: Rest<Value<'_>>| -> rquickjs::Result<()> {
            let mut parts = Vec::with_capacity(args.0.len());
            for arg in args.0 {
                parts.push(format_console_value(&arg));
            }
            logs.lock().unwrap().push(parts.join(" "));
            Ok(())
        },
    )?;

    let console = Object::new(ctx.clone())?;
    console.set("log", log_fn.clone())?;
    console.set("info", log_fn.clone())?;
    console.set("warn", log_fn.clone())?;
    console.set("error", log_fn)?;
    ctx.globals().set("console", console)?;
    Ok(())
}

/// Render a single value passed to `console.log` into text: strings as-is,
/// `undefined` as `"undefined"`, and structured values via `JSON.stringify`.
fn format_console_value<'js>(val: &Value<'js>) -> String {
    if let Some(s) = val.as_string()
        && let Ok(rust_str) = s.to_string()
    {
        return rust_str;
    }
    if val.is_undefined() {
        return "undefined".to_string();
    }
    if val.is_null() {
        return "null".to_string();
    }
    let ctx = val.ctx();
    if let Ok(Some(json)) = ctx.json_stringify(val.clone())
        && let Ok(s) = json.to_string()
    {
        return s;
    }
    format!("{val:?}")
}

/// One match entry returned by `tools.grep`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct GrepMatch {
    path: String,
    line: usize,
    text: String,
}

impl<'js> IntoJs<'js> for GrepMatch {
    fn into_js(self, ctx: &Ctx<'js>) -> rquickjs::Result<Value<'js>> {
        let obj = Object::new(ctx.clone())?;
        obj.set("path", self.path)?;
        obj.set("line", self.line)?;
        obj.set("text", self.text)?;
        Ok(obj.into_value())
    }
}

/// Install the `tools` namespace on `globalThis` according to `caps`.
fn install_tools<'js>(ctx: &Ctx<'js>, caps: &Capabilities) -> rquickjs::Result<()> {
    let tools = Object::new(ctx.clone())?;

    let read_root = caps.root.clone();
    tools.set(
        "read",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'_>,
                  path: String,
                  start_line: Opt<Option<isize>>,
                  end_line: Opt<Option<isize>>|
                  -> rquickjs::Result<String> {
                let start_line = start_line.0.flatten();
                let end_line = end_line.0.flatten();
                host_read_range(&read_root, &path, start_line, end_line)
                    .map_err(|e| Exception::throw_message(&ctx, &e))
            },
        )?,
    )?;

    let list_root = caps.root.clone();
    tools.set(
        "list",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'_>, glob: Opt<Option<String>>| -> rquickjs::Result<Vec<String>> {
                let glob = glob.0.flatten();
                host_list(&list_root, glob.as_deref())
                    .map_err(|e| Exception::throw_message(&ctx, &e))
            },
        )?,
    )?;

    let grep_root = caps.root.clone();
    tools.set(
        "grep",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'_>,
                  pattern: String,
                  glob: Opt<Option<String>>|
                  -> rquickjs::Result<Vec<GrepMatch>> {
                let glob = glob.0.flatten();
                host_grep(&grep_root, &pattern, glob.as_deref())
                    .map_err(|e| Exception::throw_message(&ctx, &e))
            },
        )?,
    )?;

    let diff_root = caps.root.clone();
    tools.set(
        "diff",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'_>| -> rquickjs::Result<String> {
                host_diff(&diff_root).map_err(|e| Exception::throw_message(&ctx, &e))
            },
        )?,
    )?;

    if caps.writable {
        let write_root = caps.root.clone();
        tools.set(
            "write",
            Function::new(
                ctx.clone(),
                move |ctx: Ctx<'_>, path: String, content: String| -> rquickjs::Result<()> {
                    host_write(&write_root, &path, &content)
                        .map_err(|e| Exception::throw_message(&ctx, &e))
                },
            )?,
        )?;

        let replace_root = caps.root.clone();
        tools.set(
            "replace",
            Function::new(
                ctx.clone(),
                move |ctx: Ctx<'_>,
                      path: String,
                      old_text: String,
                      new_text: String|
                      -> rquickjs::Result<()> {
                    host_replace(&replace_root, &path, &old_text, &new_text)
                        .map_err(|e| Exception::throw_message(&ctx, &e))
                },
            )?,
        )?;
    }

    ctx.globals().set("tools", tools)?;
    Ok(())
}

/// Resolve a relative path under `root`, rejecting empty paths, absolute paths,
/// `..` traversal components, and symlinks (file or directory) that point
/// outside the canonical project root.
pub fn resolve_confined_path(root: &Path, raw_path: &str) -> Result<PathBuf, String> {
    let trimmed = raw_path.trim();
    if trimmed.is_empty() {
        return Err("path must not be empty".to_string());
    }

    let candidate = Path::new(raw_path);
    if candidate.is_absolute() {
        return Err(format!(
            "path `{raw_path}` must be relative to the project root, not absolute"
        ));
    }

    let mut normal_components = Vec::new();
    for component in candidate.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => normal_components.push(part),
            Component::ParentDir => {
                return Err(format!(
                    "path `{raw_path}` contains `..`, which is not allowed outside the project root"
                ));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(format!(
                    "path `{raw_path}` must be relative to the project root"
                ));
            }
        }
    }

    if normal_components.is_empty() {
        return Err(format!("path `{raw_path}` does not name a file"));
    }

    let canonical_root = fs::canonicalize(root).map_err(|e| {
        format!(
            "could not canonicalize project root `{}`: {e}",
            root.display()
        )
    })?;

    let mut current = canonical_root.clone();
    for part in normal_components {
        current.push(part);
        if fs::symlink_metadata(&current).is_ok() {
            let resolved = fs::canonicalize(&current).map_err(|_| {
                format!("path `{raw_path}` escapes or cannot be resolved inside the project root")
            })?;
            if !resolved.starts_with(&canonical_root) {
                return Err(format!(
                    "path `{raw_path}` escapes the project root `{}`",
                    canonical_root.display()
                ));
            }
            current = resolved;
        }
    }

    if !current.starts_with(&canonical_root) {
        return Err(format!(
            "path `{raw_path}` escapes the project root `{}`",
            canonical_root.display()
        ));
    }

    Ok(current)
}

/// Validate an optional git pathspec/glob argument so it cannot traverse
/// outside the project root.
fn validate_glob(glob: &str) -> Result<(), String> {
    if glob.starts_with('/') {
        return Err(format!(
            "glob `{glob}` must be relative to the project root"
        ));
    }
    let path = Path::new(glob);
    for component in path.components() {
        if matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        ) {
            return Err(format!(
                "glob `{glob}` may not reference parent or root directories"
            ));
        }
    }
    Ok(())
}

/// `tools.read(path)`: read a UTF-8 file relative to `root`.
pub fn host_read(root: &Path, path: &str) -> Result<String, String> {
    host_read_range(root, path, None, None)
}

/// `tools.read(path, startLine?, endLine?)`: read a UTF-8 file relative to `root`,
/// optionally slicing 1-based inclusive line numbers `[start_line..=end_line]`.
pub fn host_read_range(
    root: &Path,
    path: &str,
    start_line: Option<isize>,
    end_line: Option<isize>,
) -> Result<String, String> {
    let resolved = resolve_confined_path(root, path)?;
    let content =
        fs::read_to_string(&resolved).map_err(|e| format!("tools.read(`{path}`): {e}"))?;

    if start_line.is_none() && end_line.is_none() {
        return Ok(content);
    }

    let start = start_line.unwrap_or(1);
    if start < 1 {
        return Err(format!(
            "tools.read(`{path}`): `startLine` must be >= 1, got {start}"
        ));
    }
    let start_idx = (start as usize) - 1;

    if let Some(end) = end_line {
        if end < 1 {
            return Err(format!(
                "tools.read(`{path}`): `endLine` must be >= 1, got {end}"
            ));
        }
        if end < start {
            return Err(format!(
                "tools.read(`{path}`): `endLine` ({end}) must be >= `startLine` ({start})"
            ));
        }
        let count = (end as usize) - start_idx;
        Ok(content
            .lines()
            .skip(start_idx)
            .take(count)
            .collect::<Vec<_>>()
            .join("\n"))
    } else {
        Ok(content
            .lines()
            .skip(start_idx)
            .collect::<Vec<_>>()
            .join("\n"))
    }
}

/// `tools.write(path, content)`: overwrite or create a UTF-8 file relative to `root`.
pub fn host_write(root: &Path, path: &str, content: &str) -> Result<(), String> {
    let resolved = resolve_confined_path(root, path)?;
    if let Some(parent) = resolved.parent() {
        fs::create_dir_all(parent).map_err(|e| {
            format!("tools.write(`{path}`): could not create parent directory: {e}")
        })?;
    }
    fs::write(&resolved, content).map_err(|e| format!("tools.write(`{path}`): {e}"))
}

/// The Unicode scalar a UTF-8 byte-order mark decodes to.
const UTF8_BOM: char = '\u{FEFF}';

/// The LF-normalized search view of a file plus the mapping from a view byte
/// boundary back to the corresponding source byte boundary.
struct LfView {
    text: String,
    source_offsets: Vec<usize>,
}

impl LfView {
    /// Build the search view for `source`, replacing each `\r\n` with `\n`
    /// and remembering where every view byte came from.
    fn new(source: &str) -> Self {
        let bytes = source.as_bytes();
        let mut text = String::with_capacity(source.len());
        let mut source_offsets = Vec::with_capacity(source.len() + 1);
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index] == b'\r' && bytes.get(index + 1) == Some(&b'\n') {
                source_offsets.push(index);
                text.push('\n');
                index += 2;
            } else {
                let ch = source[index..]
                    .chars()
                    .next()
                    .expect("index is always a char boundary");
                let width = ch.len_utf8();
                for offset in 0..width {
                    source_offsets.push(index + offset);
                }
                text.push(ch);
                index += width;
            }
        }
        source_offsets.push(bytes.len());
        Self {
            text,
            source_offsets,
        }
    }

    /// The source byte boundary corresponding to `view_offset`.
    fn source_offset(&self, view_offset: usize) -> usize {
        self.source_offsets[view_offset]
    }
}

/// `\r\n` -> `\n`, leaving every other byte alone.
fn normalize_line_feeds(text: &str) -> String {
    text.replace("\r\n", "\n")
}

/// The dominant line ending of `source`: CRLF when it has more CRLF than bare
/// LF endings, otherwise LF (ties and files without endings are LF).
fn dominant_line_ending(source: &str) -> &'static str {
    let crlf = source.matches("\r\n").count();
    let lf = source.matches('\n').count() - crlf;
    if crlf > lf { "\r\n" } else { "\n" }
}

/// Format replacement text with `line_ending`: CRLF is normalized to LF first,
/// then every LF is written in the dominant style.
fn format_replacement(new_text: &str, line_ending: &str) -> String {
    let normalized = normalize_line_feeds(new_text);
    if line_ending == "\r\n" {
        normalized.replace('\n', "\r\n")
    } else {
        normalized
    }
}

/// Non-overlapping exact occurrences of `needle` in `haystack`.
fn exact_match_ranges(haystack: &str, needle: &str) -> Vec<(usize, usize)> {
    if needle.is_empty() {
        return Vec::new();
    }
    haystack
        .match_indices(needle)
        .map(|(start, matched)| (start, start + matched.len()))
        .collect()
}

/// One line of an LF-normalized view, with both possible end offsets.
struct ViewLine<'a> {
    start: usize,
    end_with_newline: usize,
    end_without_newline: usize,
    has_newline: bool,
    text: &'a str,
}

/// Split `view` into line spans that remember whether a newline is present.
fn view_lines(view: &str) -> Vec<ViewLine<'_>> {
    let mut lines = Vec::new();
    let mut offset = 0;
    for segment in view.split_inclusive('\n') {
        let has_newline = segment.ends_with('\n');
        let text = if has_newline {
            &segment[..segment.len() - 1]
        } else {
            segment
        };
        let start = offset;
        let end_with_newline = offset + segment.len();
        let end_without_newline = start + text.len();
        lines.push(ViewLine {
            start,
            end_with_newline,
            end_without_newline,
            has_newline,
            text,
        });
        offset = end_with_newline;
    }
    lines
}

/// Non-overlapping line-based occurrences of `search` in `view`, comparing each
/// corresponding line after removing only trailing ASCII spaces and tabs.
fn trimmed_line_match_ranges(view: &str, search: &str) -> Vec<(usize, usize)> {
    let trailing_newline = search.ends_with('\n');
    let mut expected: Vec<&str> = search.split('\n').collect();
    if trailing_newline {
        expected.pop();
    }
    if expected.is_empty() {
        return Vec::new();
    }
    let expected: Vec<&str> = expected
        .iter()
        .map(|line| line.trim_end_matches([' ', '\t']))
        .collect();

    let lines = view_lines(view);
    if lines.len() < expected.len() {
        return Vec::new();
    }

    let mut ranges = Vec::new();
    for start_line in 0..=(lines.len() - expected.len()) {
        let window = &lines[start_line..start_line + expected.len()];
        let all_match = window
            .iter()
            .zip(&expected)
            .all(|(line, wanted)| line.text.trim_end_matches([' ', '\t']) == *wanted);
        if !all_match {
            continue;
        }
        let last = &window[window.len() - 1];
        if trailing_newline && !last.has_newline {
            continue;
        }
        let start = window[0].start;
        let end = if trailing_newline {
            last.end_with_newline
        } else {
            last.end_without_newline
        };
        if ranges
            .last()
            .is_some_and(|&(_, previous_end)| previous_end > start)
        {
            continue;
        }
        ranges.push((start, end));
    }
    ranges
}

/// The 1-based source line on which the occurrence starting at `offset` begins.
fn start_line_number(view: &str, offset: usize) -> usize {
    1 + view.as_bytes()[..offset]
        .iter()
        .filter(|&&byte| byte == b'\n')
        .count()
}

/// `tools.replace(path, old_text, new_text)`: replace exactly one occurrence of
/// `old_text` with `new_text` in `path`. Errors on zero or more than one match.
pub fn host_replace(root: &Path, path: &str, old_text: &str, new_text: &str) -> Result<(), String> {
    if old_text.is_empty() {
        return Err(format!(
            "tools.replace(`{path}`): `old_text` must not be empty"
        ));
    }
    if old_text == new_text {
        return Err(format!(
            "tools.replace(`{path}`): `old_text` and `new_text` must differ"
        ));
    }

    let resolved = resolve_confined_path(root, path)?;
    let content =
        fs::read_to_string(&resolved).map_err(|e| format!("tools.replace(`{path}`): {e}"))?;

    let (bom, body) = match content.strip_prefix(UTF8_BOM) {
        Some(body) => (&content[..UTF8_BOM.len_utf8()], body),
        None => ("", content.as_str()),
    };

    let view = LfView::new(body);
    let search = normalize_line_feeds(old_text);
    let mut ranges = exact_match_ranges(&view.text, &search);
    if ranges.is_empty() {
        ranges = trimmed_line_match_ranges(&view.text, &search);
    }

    if ranges.is_empty() {
        return Err(format!(
            "tools.replace(`{path}`): `old_text` matched 0 occurrences; expected exactly 1"
        ));
    }

    if ranges.len() > 1 {
        let matches = ranges.len();
        let starts: Vec<String> = ranges
            .iter()
            .map(|&(start, _)| start_line_number(&view.text, start).to_string())
            .collect();
        let listed = if matches <= 5 {
            format!("at lines {}", starts.join(", "))
        } else {
            format!("first 5 at lines {}", starts[..5].join(", "))
        };
        return Err(format!(
            "tools.replace(`{path}`): `old_text` matched {matches} occurrences; expected exactly 1 ({listed}; add surrounding context lines)"
        ));
    }

    let (start, end) = ranges[0];
    let source_start = view.source_offset(start);
    let source_end = view.source_offset(end);
    let replacement = format_replacement(new_text, dominant_line_ending(body));

    let mut updated = String::with_capacity(bom.len() + body.len() + replacement.len());
    updated.push_str(bom);
    updated.push_str(&body[..source_start]);
    updated.push_str(&replacement);
    updated.push_str(&body[source_end..]);

    fs::write(&resolved, updated).map_err(|e| format!("tools.replace(`{path}`): {e}"))
}

/// `tools.list(glob?)`: list tracked and untracked-not-ignored files using
/// `git ls-files --cached --others --exclude-standard`.
///
/// The `.ejaj/` run artifacts are excluded -- whether they are tracked or
/// untracked, and whatever glob the caller passes -- so discovery never
/// reveals the run's own handoffs and spill logs. With no caller glob, the
/// whole project root is the positive pathspec the exclusion is subtracted
/// from; a caller glob replaces it and therefore keeps narrowing the listing.
pub fn host_list(root: &Path, glob: Option<&str>) -> Result<Vec<String>, String> {
    let mut cmd = Command::new("git");
    cmd.current_dir(root)
        .args(["ls-files", "--cached", "--others", "--exclude-standard"]);

    cmd.arg("--");
    match glob.filter(|g| !g.is_empty()) {
        Some(pattern) => {
            validate_glob(pattern)?;
            cmd.arg(pattern);
        }
        None => {
            cmd.arg(WHOLE_TREE_PATHSPEC);
        }
    }
    cmd.arg(RUN_ARTIFACT_PATHSPEC);

    let output = cmd
        .output()
        .map_err(|e| format!("tools.list: failed to run `git ls-files`: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "tools.list: `git ls-files` exited with {}: {}",
            output.status,
            stderr.trim()
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout
        .lines()
        .filter(|line| !line.is_empty())
        .filter(|line| !is_run_artifact(line))
        .map(str::to_string)
        .collect())
}

/// `tools.grep(pattern, glob?)`: search tracked and untracked-not-ignored files
/// using `git grep`.
///
/// The `.ejaj/` run artifacts are excluded -- tracked or untracked, and
/// whatever glob the caller passes -- so search never returns the run's own
/// handoffs and spill logs. With no caller glob, the whole project root is the
/// positive pathspec the exclusion is subtracted from; an ordinary glob
/// replaces it and therefore keeps narrowing the search.
///
/// The output is NUL-delimited (`-z`): each record carries the path, the line
/// number and the matched text separated by NUL characters, so a colon in
/// either the path or the text is never mistaken for a field separator.
///
/// At most [`GREP_MATCH_LIMIT`] matches are materialized: parsing stops as soon
/// as that many valid, non-artifact records have been collected, so the first
/// matches in `git grep` order are the ones returned. Each match's source-line
/// content is independently capped at [`GREP_LINE_MAX_BYTES`] on a UTF-8
/// character boundary with an exact additive omission marker.
fn host_grep(root: &Path, pattern: &str, glob: Option<&str>) -> Result<Vec<GrepMatch>, String> {
    if pattern.is_empty() {
        return Err("tools.grep: `pattern` must not be empty".to_string());
    }

    let mut cmd = Command::new("git");
    cmd.current_dir(root).args([
        "grep",
        "-n",
        "-z",
        "--untracked",
        "-I",
        "--no-color",
        "-e",
        pattern,
    ]);

    cmd.arg("--");
    match glob.filter(|g| !g.is_empty()) {
        Some(glob_pattern) => {
            validate_glob(glob_pattern)?;
            cmd.arg(glob_pattern);
        }
        None => {
            cmd.arg(WHOLE_TREE_PATHSPEC);
        }
    }
    cmd.arg(RUN_ARTIFACT_PATHSPEC);

    let output = cmd
        .output()
        .map_err(|e| format!("tools.grep: failed to run `git grep`: {e}"))?;

    match output.status.code() {
        Some(0) => {}
        // `git grep` exits 1 when no lines matched.
        Some(1) => return Ok(Vec::new()),
        _ => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!(
                "tools.grep: `git grep` exited with {}: {}",
                output.status,
                stderr.trim()
            ));
        }
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut results = Vec::new();
    for line in stdout.lines() {
        // Stop materializing records as soon as the cap is reached: the
        // remaining output is never parsed and no further `GrepMatch` is built.
        if results.len() >= GREP_MATCH_LIMIT {
            break;
        }
        if let Some((path, rest)) = line.split_once('\0')
            && let Some((line_no, text)) = rest.split_once('\0')
            && let Ok(parsed_line) = line_no.parse::<usize>()
            && !is_run_artifact(path)
        {
            results.push(GrepMatch {
                path: path.to_string(),
                line: parsed_line,
                text: cap_grep_line(text, GREP_LINE_MAX_BYTES),
            });
        }
    }
    Ok(results)
}

/// Retain at most `limit` UTF-8 bytes of one matched line, stopping at a
/// character boundary, and append the exact additive omission marker when the
/// source line is longer. The marker is not part of the retained-content cap.
fn cap_grep_line(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let end = floor_char_boundary(text, limit);
    let omitted = text.len() - end;
    format!(
        "{}... [line truncated to {limit} bytes; +{omitted} bytes]",
        &text[..end]
    )
}

/// The largest untracked file rendered as a diff, in bytes. A bigger file is
/// named rather than carried: the model reads the change set, not a database.
const UNTRACKED_FILE_LIMIT_BYTES: usize = 256 * 1024;

/// One git command rooted at `root`, with optional locking disabled so a
/// read-only binding can never rewrite the index it only meant to read.
fn git_read(root: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(root).env("GIT_OPTIONAL_LOCKS", "0");
    cmd
}

/// Run one git subcommand for the change set and return its stdout, or its
/// stderr as the error handed back to the model.
fn git_stdout(root: &Path, args: &[&str]) -> Result<String, String> {
    let output = git_read(root)
        .args(args)
        .output()
        .map_err(|e| format!("tools.diff: failed to run `git {}`: {e}", args.join(" ")))?;

    if !output.status.success() {
        return Err(format!(
            "tools.diff: `git {}` exited with {}: {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Whether a repository-relative `path` is part of the `.ejaj/` run artifacts --
/// the snapshots, reports and handoff documents of the run itself -- which the
/// change set must never contain.
fn is_run_artifact(path: &str) -> bool {
    path == ".ejaj" || path.starts_with(".ejaj/")
}

/// The pathspec that keeps every `.ejaj/` run artifact out of discovery and
/// diff output.
const RUN_ARTIFACT_PATHSPEC: &str = ":(exclude).ejaj";

/// The positive pathspec paired with the artifact exclusion when a discovery
/// command has no caller glob: the whole project root, minus run artifacts.
const WHOLE_TREE_PATHSPEC: &str = ".";

/// `tools.diff()`: the repository-root change set as unified diff text.
///
/// The change set is the staged and unstaged modifications of tracked files
/// plus every non-ignored untracked file, and nothing else: ignored paths are
/// filtered by `git ls-files --others --exclude-standard`, and the `.ejaj/` run
/// artifacts are excluded from both halves. The binding only reads -- every git
/// command runs with `GIT_OPTIONAL_LOCKS=0`, so even the index is left exactly
/// as it was -- and nothing is staged, stashed or written.
pub fn host_diff(root: &Path) -> Result<String, String> {
    let mut text = tracked_diff(root)?;
    for path in untracked_paths(root)? {
        if is_run_artifact(&path) {
            continue;
        }
        text.push_str(&new_file_diff(root, &path));
    }
    Ok(text)
}

/// Every tracked change -- staged and unstaged -- as one unified diff against
/// `HEAD`. A repository with no commit yet has no `HEAD` to compare with, so
/// the staged and the unstaged halves are asked for separately instead.
fn tracked_diff(root: &Path) -> Result<String, String> {
    let has_head = git_read(root)
        .args(["rev-parse", "--verify", "--quiet", "HEAD"])
        .output()
        .map_err(|e| format!("tools.diff: failed to run `git rev-parse`: {e}"))?
        .status
        .success();

    if has_head {
        return git_stdout(
            root,
            &[
                "diff",
                "HEAD",
                "--no-color",
                "--no-ext-diff",
                "--",
                RUN_ARTIFACT_PATHSPEC,
            ],
        );
    }

    let mut text = git_stdout(
        root,
        &[
            "diff",
            "--cached",
            "--no-color",
            "--no-ext-diff",
            "--",
            RUN_ARTIFACT_PATHSPEC,
        ],
    )?;
    text.push_str(&git_stdout(
        root,
        &[
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--",
            RUN_ARTIFACT_PATHSPEC,
        ],
    )?);
    Ok(text)
}

/// Every untracked file that is not ignored, as `git ls-files --others
/// --exclude-standard` reports it. The list is read NUL-separated so a path
/// with a space or a non-ASCII character survives verbatim.
fn untracked_paths(root: &Path) -> Result<Vec<String>, String> {
    let stdout = git_stdout(root, &["ls-files", "--others", "--exclude-standard", "-z"])?;
    Ok(stdout
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(str::to_string)
        .collect())
}

/// One untracked file rendered as a new-file unified diff, so the model reads
/// its content and not only its name. Binary and oversized files are named
/// rather than carried.
fn new_file_diff(root: &Path, path: &str) -> String {
    let header = format!("diff --git a/{path} b/{path}\n");
    let resolved = root.join(path);

    if let Ok(metadata) = fs::metadata(&resolved)
        && metadata.len() > UNTRACKED_FILE_LIMIT_BYTES as u64
    {
        return format!(
            "{header}--- /dev/null\n+++ b/{path}\n@@ -0,0 +1 @@\n\
             +[new file omitted: {} bytes, larger than the {UNTRACKED_FILE_LIMIT_BYTES}-byte \
             diff limit]\n",
            metadata.len()
        );
    }

    let Ok(bytes) = fs::read(&resolved) else {
        return format!("{header}[unreadable new file: {path}]\n");
    };
    let Ok(content) = String::from_utf8(bytes) else {
        return format!("{header}Binary files /dev/null and b/{path} differ\n");
    };

    let mut section = format!("{header}--- /dev/null\n+++ b/{path}\n");
    if content.is_empty() {
        return section;
    }

    let ends_with_newline = content.ends_with('\n');
    let mut lines: Vec<&str> = content.split('\n').collect();
    if ends_with_newline {
        lines.pop();
    }
    section.push_str(&format!("@@ -0,0 +1,{} @@\n", lines.len()));
    for line in lines {
        section.push('+');
        section.push_str(line);
        section.push('\n');
    }
    if !ends_with_newline {
        section.push_str("\\ No newline at end of file\n");
    }
    section
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// Create a fresh temporary directory for one test.
    fn scratch_dir(label: &str) -> PathBuf {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "ejaj-script-test-{}-{id}-{label}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("temp dir should be creatable");
        dir
    }

    /// Initialize a tiny git repository in `dir` so `tools.list` and
    /// `tools.grep` can run in complete isolation from the main repository.
    fn init_git_repo(dir: &Path) {
        let status = Command::new("git")
            .current_dir(dir)
            .args(["init", "-q"])
            .status()
            .expect("git init should run");
        assert!(status.success(), "git init failed");
    }

    /// Run one git command in `dir`, failing the test if it does not exit 0.
    fn run_git(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
            .current_dir(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "ejaj tests")
            .env("GIT_AUTHOR_EMAIL", "ejaj@example.com")
            .env("GIT_COMMITTER_NAME", "ejaj tests")
            .env("GIT_COMMITTER_EMAIL", "ejaj@example.com")
            .output()
            .expect("git is installed and runs");
        assert!(
            output.status.success(),
            "`git {}` failed ({}): {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// The repository state a read-only binding must not change: the porcelain
    /// status, which names both the staged and the unstaged half of every path.
    fn repo_state(dir: &Path) -> String {
        let output = Command::new("git")
            .current_dir(dir)
            .args(["status", "--porcelain", "--untracked-files=all"])
            .output()
            .expect("git is installed and runs");
        assert!(output.status.success(), "`git status --porcelain` failed");
        String::from_utf8_lossy(&output.stdout).to_string()
    }

    /// `tools.diff()` over an isolated repository: the staged and the unstaged
    /// tracked changes and the non-ignored untracked files are carried, the
    /// ignored paths and the `.ejaj/` run artifacts are not, and the repository
    /// -- working tree, index and status -- is left exactly as it was.
    #[test]
    fn diff_carries_the_change_set_and_leaves_the_repository_untouched() {
        let dir = scratch_dir("diff-change-set");
        init_git_repo(&dir);
        fs::write(dir.join(".gitignore"), "ignored.txt\n").unwrap();
        fs::write(dir.join("tracked.txt"), "one\n").unwrap();
        fs::write(dir.join("staged.txt"), "before\n").unwrap();
        run_git(&dir, &["add", "-A"]);
        run_git(&dir, &["commit", "-q", "-m", "initial"]);

        // An unstaged change to a tracked file.
        fs::write(dir.join("tracked.txt"), "one\ntwo\n").unwrap();
        // A staged change to a tracked file.
        fs::write(dir.join("staged.txt"), "staged content\n").unwrap();
        run_git(&dir, &["add", "staged.txt"]);
        // An untracked file that is not ignored.
        fs::write(dir.join("untracked.txt"), "untracked content\n").unwrap();
        // An untracked file that is ignored.
        fs::write(dir.join("ignored.txt"), "ignored content\n").unwrap();
        // Untracked run artifacts under `.ejaj/`.
        fs::create_dir_all(dir.join(".ejaj/run")).unwrap();
        fs::write(dir.join(".ejaj/run/queue-report.md"), "run artifact\n").unwrap();

        let before = repo_state(&dir);
        let engine = QuickJsEngine::new();
        let caps = Capabilities::read_only(&dir);

        let first = engine
            .run("return tools.diff();", &caps)
            .expect("tools.diff should succeed");
        let second = engine
            .run("return tools.diff();", &caps)
            .expect("tools.diff should succeed");

        assert!(
            first.contains("tracked.txt") && first.contains("+two"),
            "the unstaged tracked change is missing: {first}"
        );
        assert!(
            first.contains("staged.txt") && first.contains("+staged content"),
            "the staged tracked change is missing: {first}"
        );
        assert!(
            first.contains("untracked.txt") && first.contains("+untracked content"),
            "the untracked file is missing: {first}"
        );
        assert!(
            !first.contains("ignored.txt"),
            "an ignored path reached the change set: {first}"
        );
        assert!(
            !first.contains(".ejaj"),
            "a `.ejaj/` run artifact reached the change set: {first}"
        );

        assert_eq!(first, second, "the change set must be stable across calls");
        assert_eq!(
            repo_state(&dir),
            before,
            "tools.diff modified the repository"
        );
        assert_eq!(
            fs::read_to_string(dir.join("tracked.txt")).unwrap(),
            "one\ntwo\n",
            "tools.diff rewrote a working-tree file"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// The spill area the Bash handler writes under `.ejaj/` is a run artifact,
    /// not a change: it never reaches the change set, while an ordinary
    /// untracked file beside it still does.
    #[test]
    fn when_repository_diff_given_spill_files_and_visible_changes_then_excludes_only_ignored_spill_area()
     {
        let dir = scratch_dir("diff-spill");
        init_git_repo(&dir);
        fs::write(dir.join(".gitignore"), ".ejaj/\n").unwrap();
        fs::write(dir.join("tracked.txt"), "before\n").unwrap();
        run_git(&dir, &["add", "-A"]);
        run_git(&dir, &["commit", "-q", "-m", "initial"]);

        fs::write(dir.join("tracked.txt"), "before\nvisible change\n").unwrap();
        fs::create_dir_all(dir.join(".ejaj/tool-output")).unwrap();
        fs::write(
            dir.join(".ejaj/tool-output/step-1-call-1.log"),
            "the complete raw bash output\n",
        )
        .unwrap();
        fs::write(dir.join("ordinary.txt"), "ordinary untracked\n").unwrap();

        let diff = host_diff(&dir).expect("tools.diff should succeed");

        assert!(
            diff.contains("tracked.txt") && diff.contains("+visible change"),
            "the ordinary tracked change is missing: {diff}"
        );
        assert!(
            diff.contains("ordinary.txt") && diff.contains("+ordinary untracked"),
            "the ordinary untracked file is missing: {diff}"
        );
        assert!(
            !diff.contains(".ejaj") && !diff.contains("step-1-call-1.log"),
            "the ignored spill area reached the change set: {diff}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// A repository with no commit yet has no `HEAD` to compare with: the staged
    /// half and the unstaged half are asked for separately and both are carried.
    #[test]
    fn diff_works_before_the_first_commit() {
        let dir = scratch_dir("diff-no-head");
        init_git_repo(&dir);
        fs::write(dir.join("staged.txt"), "staged content\n").unwrap();
        run_git(&dir, &["add", "staged.txt"]);
        fs::write(dir.join("untracked.txt"), "untracked content\n").unwrap();

        let engine = QuickJsEngine::new();
        let caps = Capabilities::read_only(&dir);
        let text = engine
            .run("return tools.diff();", &caps)
            .expect("tools.diff should succeed");

        assert!(text.contains("+staged content"), "{text}");
        assert!(text.contains("+untracked content"), "{text}");

        let _ = fs::remove_dir_all(&dir);
    }

    /// Outside a git repository the binding reports a useful error instead of a
    /// panic or an empty change set.
    #[test]
    fn diff_reports_a_useful_error_outside_a_repository() {
        let dir = scratch_dir("diff-no-repo");
        let engine = QuickJsEngine::new();
        let caps = Capabilities::read_only(&dir);

        let error = engine
            .run("return tools.diff();", &caps)
            .expect_err("tools.diff must fail outside a git repository");
        assert!(
            error.starts_with("[ERROR: execute_script failed]"),
            "unexpected error shape: {error}"
        );
        assert!(error.contains("tools.diff"), "{error}");

        let _ = fs::remove_dir_all(&dir);
    }
    #[test]
    fn defined_return_value_is_json_stringified() {
        let dir = scratch_dir("return-json");
        let engine = QuickJsEngine::new();
        let caps = Capabilities::read_only(&dir);

        let out = engine
            .run("({ answer: 40 + 2, ok: true })", &caps)
            .expect("script should succeed");
        assert_eq!(out, r#"{"answer":42,"ok":true}"#);

        let out_return = engine
            .run("const a = [1, 2, 3]; return a.map(x => x * 2);", &caps)
            .expect("top-level return should succeed");
        assert_eq!(out_return, "[2,4,6]");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn undefined_return_falls_back_to_console_log_buffer() {
        let dir = scratch_dir("console-log");
        let engine = QuickJsEngine::new();
        let caps = Capabilities::read_only(&dir);

        let out = engine
            .run(
                r#"console.log("first", 1); console.log({ second: true });"#,
                &caps,
            )
            .expect("script should succeed");
        assert_eq!(out, "first 1\n{\"second\":true}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn defined_return_takes_precedence_over_console_log() {
        let dir = scratch_dir("return-precedence");
        let engine = QuickJsEngine::new();
        let caps = Capabilities::read_only(&dir);

        let out = engine
            .run(r#"console.log("noise"); return "distilled";"#, &caps)
            .expect("script should succeed");
        assert_eq!(out, "distilled");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn ephemeral_scope_leaves_no_globals_between_calls() {
        let dir = scratch_dir("ephemeral");
        let engine = QuickJsEngine::new();
        let caps = Capabilities::read_only(&dir);

        engine
            .run("globalThis.leaked = 99; return globalThis.leaked;", &caps)
            .expect("first script should succeed");
        let second = engine
            .run("return typeof globalThis.leaked;", &caps)
            .expect("second script should succeed");
        assert_eq!(second, "undefined");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_write_and_replace_round_trip_in_code_phase() {
        let dir = scratch_dir("rw-replace");
        let engine = QuickJsEngine::new();
        let caps = Capabilities::read_write(&dir);

        let result = engine
            .run(
                r#"
                tools.write("sub/hello.txt", "hello world\ngoodbye world\n");
                tools.replace("sub/hello.txt", "hello world", "hi world");
                return tools.read("sub/hello.txt");
                "#,
                &caps,
            )
            .expect("script should succeed");
        assert_eq!(result, "hi world\ngoodbye world\n");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_supports_optional_one_based_inclusive_line_ranges() {
        let dir = scratch_dir("read-range");
        fs::write(dir.join("lines.txt"), "one\ntwo\nthree\nfour\nfive\n").unwrap();
        let engine = QuickJsEngine::new();
        let caps = Capabilities::read_only(&dir);

        let middle = engine
            .run(r#"return tools.read("lines.txt", 2, 4);"#, &caps)
            .expect("range read should succeed");
        assert_eq!(middle, "two\nthree\nfour");

        let from_three = engine
            .run(r#"return tools.read("lines.txt", 3);"#, &caps)
            .expect("open-ended range read should succeed");
        assert_eq!(from_three, "three\nfour\nfive");

        let up_to_two = engine
            .run(r#"return tools.read("lines.txt", null, 2);"#, &caps)
            .expect("head range read should succeed");
        assert_eq!(up_to_two, "one\ntwo");

        let bad_start = engine
            .run(r#"return tools.read("lines.txt", 0, 2);"#, &caps)
            .expect_err("startLine < 1 must fail");
        assert!(bad_start.contains("`startLine` must be >= 1"));

        let inverted = engine
            .run(r#"return tools.read("lines.txt", 4, 2);"#, &caps)
            .expect_err("endLine < startLine must fail");
        assert!(inverted.contains("`endLine` (2) must be >= `startLine` (4)"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn when_replacing_text_given_multiple_occurrences_then_reports_one_based_line_numbers() {
        let dir = scratch_dir("replace-line-numbers");
        let engine = QuickJsEngine::new();
        let caps = Capabilities::read_write(&dir);

        // Two occurrences on separate lines.
        fs::write(dir.join("pair.txt"), "one\ntwo one\n").unwrap();
        let pair = engine
            .run(r#"tools.replace("pair.txt", "one", "replacement");"#, &caps)
            .expect_err("two occurrences must fail");
        assert!(
            pair.contains(
                "tools.replace(`pair.txt`): `old_text` matched 2 occurrences; expected exactly 1 (at lines 1, 2; add surrounding context lines)"
            ),
            "unexpected two-occurrence diagnostics: {pair}"
        );

        // Three occurrences: two start on line 1 and the third on line 3.
        fs::write(dir.join("triple.txt"), "token token\nmiddle\ntoken\n").unwrap();
        let triple = engine
            .run(
                r#"tools.replace("triple.txt", "token", "replacement");"#,
                &caps,
            )
            .expect_err("three occurrences must fail");
        assert!(
            triple.contains(
                "tools.replace(`triple.txt`): `old_text` matched 3 occurrences; expected exactly 1 (at lines 1, 1, 3; add surrounding context lines)"
            ),
            "unexpected three-occurrence diagnostics: {triple}"
        );

        // Four occurrences on lines 1, 4, 7 and 10.
        fs::write(
            dir.join("quad.txt"),
            "alpha\nbeta\ngamma\nalpha\nbeta\ngamma\nalpha\nbeta\ngamma\nalpha\n",
        )
        .unwrap();
        let quad = engine
            .run(
                r#"tools.replace("quad.txt", "alpha", "replacement");"#,
                &caps,
            )
            .expect_err("four occurrences must fail");
        assert!(
            quad.contains(
                "tools.replace(`quad.txt`): `old_text` matched 4 occurrences; expected exactly 1 (at lines 1, 4, 7, 10; add surrounding context lines)"
            ),
            "unexpected four-occurrence diagnostics: {quad}"
        );

        // Five occurrences on one line each contribute that line.
        fs::write(dir.join("quint.txt"), "x x x x x\n").unwrap();
        let quint = engine
            .run(r#"tools.replace("quint.txt", "x", "replacement");"#, &caps)
            .expect_err("five occurrences must fail");
        assert!(
            quint.contains(
                "tools.replace(`quint.txt`): `old_text` matched 5 occurrences; expected exactly 1 (at lines 1, 1, 1, 1, 1; add surrounding context lines)"
            ),
            "unexpected five-occurrence diagnostics: {quint}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn when_replacing_text_given_more_than_five_occurrences_then_reports_only_first_five_lines() {
        let dir = scratch_dir("replace-first-five");
        let engine = QuickJsEngine::new();
        let caps = Capabilities::read_write(&dir);

        fs::write(dir.join("many.txt"), "title\nx\nx\nx\nx\nx\nx\nx\n").unwrap();
        let many = engine
            .run(r#"tools.replace("many.txt", "x", "replacement");"#, &caps)
            .expect_err("seven occurrences must fail");
        assert!(
            many.contains(
                "tools.replace(`many.txt`): `old_text` matched 7 occurrences; expected exactly 1 (first 5 at lines 2, 3, 4, 5, 6; add surrounding context lines)"
            ),
            "unexpected capped diagnostics: {many}"
        );
        assert!(
            !many.contains(", 7"),
            "line numbers after the first five must be omitted: {many}"
        );

        // More than five starts on one line also cap at the first five entries.
        fs::write(dir.join("dense.txt"), "y y y y y y\n").unwrap();
        let dense = engine
            .run(r#"tools.replace("dense.txt", "y", "replacement");"#, &caps)
            .expect_err("six occurrences must fail");
        assert!(
            dense.contains(
                "tools.replace(`dense.txt`): `old_text` matched 6 occurrences; expected exactly 1 (first 5 at lines 1, 1, 1, 1, 1; add surrounding context lines)"
            ),
            "unexpected dense capped diagnostics: {dense}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn when_replacing_text_given_duplicate_occurrences_then_leaves_file_bytes_unchanged() {
        let dir = scratch_dir("replace-duplicate-bytes");
        let engine = QuickJsEngine::new();
        let caps = Capabilities::read_write(&dir);

        // The two "aa" matches inside "aaa aaa" are selected non-overlapping
        // and both start on line 1.
        let uncapped_original = b"aaa aaa\nzzz\r\n".to_vec();
        fs::write(dir.join("uncapped.txt"), &uncapped_original).unwrap();
        let uncapped = engine
            .run(
                r#"tools.replace("uncapped.txt", "aa", "replacement");"#,
                &caps,
            )
            .expect_err("two non-overlapping occurrences must fail");
        assert!(
            uncapped.contains(
                "tools.replace(`uncapped.txt`): `old_text` matched 2 occurrences; expected exactly 1 (at lines 1, 1; add surrounding context lines)"
            ),
            "unexpected uncapped diagnostics: {uncapped}"
        );
        assert_eq!(
            fs::read(dir.join("uncapped.txt")).unwrap(),
            uncapped_original,
            "an uncapped duplicate failure rewrote the file"
        );

        let capped_original = b"hit\nhit\nhit\nhit\nhit\nhit\r\nhit\n".to_vec();
        fs::write(dir.join("capped.txt"), &capped_original).unwrap();
        let capped = engine
            .run(
                r#"tools.replace("capped.txt", "hit", "replacement");"#,
                &caps,
            )
            .expect_err("seven occurrences must fail");
        assert!(
            capped.contains(
                "tools.replace(`capped.txt`): `old_text` matched 7 occurrences; expected exactly 1 (first 5 at lines 1, 2, 3, 4, 5; add surrounding context lines)"
            ),
            "unexpected capped diagnostics: {capped}"
        );
        assert_eq!(
            fs::read(dir.join("capped.txt")).unwrap(),
            capped_original,
            "a capped duplicate failure rewrote the file"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn when_replacing_text_given_duplicate_occurrences_then_preserves_existing_count_and_access_contracts()
     {
        let dir = scratch_dir("replace-contracts");
        let engine = QuickJsEngine::new();
        let caps = Capabilities::read_write(&dir);

        fs::write(dir.join("exact.txt"), "alpha beta alpha\n").unwrap();
        let zero = engine
            .run(r#"tools.replace("exact.txt", "missing", "new");"#, &caps)
            .expect_err("zero occurrences must fail");
        assert!(
            zero.contains(
                "tools.replace(`exact.txt`): `old_text` matched 0 occurrences; expected exactly 1"
            ),
            "unexpected zero-match error: {zero}"
        );

        let many = engine
            .run(r#"tools.replace("exact.txt", "alpha", "gamma");"#, &caps)
            .expect_err("multiple exact occurrences must fail");
        assert!(
            many.contains(
                "tools.replace(`exact.txt`): `old_text` matched 2 occurrences; expected exactly 1 (at lines 1, 1; add surrounding context lines)"
            ),
            "unexpected exact multi-match error: {many}"
        );
        assert_eq!(
            fs::read_to_string(dir.join("exact.txt")).unwrap(),
            "alpha beta alpha\n"
        );

        // The whitespace fallback enforces uniqueness too, using the selected
        // tier's starts for the line diagnostics.
        fs::write(dir.join("fallback.txt"), "alpha  \nheader\nalpha\t\n").unwrap();
        let fallback_many = engine
            .run(
                r#"tools.replace("fallback.txt", "alpha\n", "gamma\n");"#,
                &caps,
            )
            .expect_err("multiple fallback occurrences must fail");
        assert!(
            fallback_many.contains(
                "tools.replace(`fallback.txt`): `old_text` matched 2 occurrences; expected exactly 1 (at lines 1, 3; add surrounding context lines)"
            ),
            "unexpected fallback multi-match error: {fallback_many}"
        );
        assert_eq!(
            fs::read_to_string(dir.join("fallback.txt")).unwrap(),
            "alpha  \nheader\nalpha\t\n"
        );

        // Read-only capabilities never expose replace.
        let read_only = Capabilities::read_only(&dir);
        let denied = engine
            .run(
                r#"tools.replace("exact.txt", "alpha", "gamma");"#,
                &read_only,
            )
            .expect_err("read-only capabilities must not expose replace");
        assert!(
            denied.starts_with("[ERROR: execute_script failed]"),
            "unexpected read-only error prefix: {denied}"
        );

        // Path confinement still rejects escapes before any write.
        let escaped = engine
            .run(
                r#"tools.replace("../outside.txt", "alpha", "gamma");"#,
                &caps,
            )
            .expect_err("a path outside the confined root must be rejected");
        assert!(
            escaped.contains("contains `..`"),
            "unexpected confinement error: {escaped}"
        );
        assert_eq!(
            fs::read_to_string(dir.join("exact.txt")).unwrap(),
            "alpha beta alpha\n"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn when_replacing_text_given_crlf_file_and_lf_old_text_then_matches_and_preserves_crlf_endings()
    {
        let dir = scratch_dir("replace-crlf");
        fs::write(
            dir.join("crlf.txt"),
            "first line\r\nsecond line\r\nthird line\r\n",
        )
        .unwrap();
        let engine = QuickJsEngine::new();
        let caps = Capabilities::read_write(&dir);

        engine
            .run(
                r#"tools.replace("crlf.txt", "second line\nthird line", "SECOND\nTHIRD");"#,
                &caps,
            )
            .expect("LF search text must match the CRLF file");

        let persisted = fs::read(dir.join("crlf.txt")).unwrap();
        assert_eq!(
            String::from_utf8(persisted).expect("replacement output is UTF-8"),
            "first line\r\nSECOND\r\nTHIRD\r\n",
            "the replaced region and the untouched region must both keep CRLF endings"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn when_replacing_text_given_identical_old_and_new_text_then_returns_identical_error() {
        let dir = scratch_dir("replace-identical");
        fs::write(dir.join("same.txt"), "alpha\n").unwrap();
        let engine = QuickJsEngine::new();
        let caps = Capabilities::read_write(&dir);

        let err = engine
            .run(r#"tools.replace("same.txt", "alpha", "alpha");"#, &caps)
            .expect_err("identical old and new text must fail");
        assert!(
            err.starts_with("[ERROR: execute_script failed]"),
            "unexpected prefix: {err}"
        );
        assert!(
            err.contains("tools.replace(`same.txt`): `old_text` and `new_text` must differ"),
            "unexpected identical-text error: {err}"
        );
        assert_eq!(fs::read_to_string(dir.join("same.txt")).unwrap(), "alpha\n");

        // The identical-text guard runs before the file is touched, so a
        // missing file still reports the no-op error rather than a read error.
        let missing = engine
            .run(r#"tools.replace("missing.txt", "alpha", "alpha");"#, &caps)
            .expect_err("identical old and new text must fail before file access");
        assert!(
            missing.contains("`old_text` and `new_text` must differ"),
            "expected the identical-text error, got: {missing}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn when_replacing_text_given_utf8_bom_then_preserves_bom_on_write() {
        let dir = scratch_dir("replace-bom");
        fs::write(dir.join("bom.txt"), "\u{FEFF}alpha\nbeta\n").unwrap();
        let engine = QuickJsEngine::new();
        let caps = Capabilities::read_write(&dir);

        engine
            .run(r#"tools.replace("bom.txt", "alpha", "omega");"#, &caps)
            .expect("replacement after the BOM must succeed");

        let persisted = fs::read_to_string(dir.join("bom.txt")).unwrap();
        assert_eq!(persisted, "\u{FEFF}omega\nbeta\n");
        assert_eq!(
            persisted.matches('\u{FEFF}').count(),
            1,
            "the BOM must remain present exactly once: {persisted:?}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn when_replacing_text_given_zero_or_empty_old_text_then_preserves_existing_error_messages() {
        let dir = scratch_dir("replace-empty-unmatched");
        fs::write(dir.join("sample.txt"), "alpha\n").unwrap();
        let engine = QuickJsEngine::new();
        let caps = Capabilities::read_write(&dir);

        // Empty old_text takes precedence even when new_text is also empty.
        let empty = engine
            .run(r#"tools.replace("sample.txt", "", "");"#, &caps)
            .expect_err("empty old_text must fail");
        assert!(
            empty.contains("tools.replace(`sample.txt`): `old_text` must not be empty"),
            "unexpected empty old_text error: {empty}"
        );

        let mismatch = engine
            .run(r#"tools.replace("sample.txt", "missing", "new");"#, &caps)
            .expect_err("an unmatched old_text must fail");
        assert!(
            mismatch.contains(
                "tools.replace(`sample.txt`): `old_text` matched 0 occurrences; expected exactly 1"
            ),
            "unexpected zero-match error: {mismatch}"
        );

        assert_eq!(
            fs::read_to_string(dir.join("sample.txt")).unwrap(),
            "alpha\n"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn when_replacing_text_given_invalid_utf8_file_then_returns_read_error_without_writing() {
        let dir = scratch_dir("replace-invalid-utf8");
        let original = vec![0xFF, 0xFE, b'a', b'\n'];
        fs::write(dir.join("binary.txt"), &original).unwrap();
        let engine = QuickJsEngine::new();
        let caps = Capabilities::read_write(&dir);

        let err = engine
            .run(r#"tools.replace("binary.txt", "a", "b");"#, &caps)
            .expect_err("invalid UTF-8 must fail");
        assert!(
            err.contains("tools.replace(`binary.txt`): "),
            "expected the tools.replace path prefix: {err}"
        );
        assert_eq!(fs::read(dir.join("binary.txt")).unwrap(), original);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn when_replacing_text_given_trailing_whitespace_difference_then_matches_via_line_trimmed_fallback()
     {
        let dir = scratch_dir("replace-fallback");
        fs::write(
            dir.join("fallback.txt"),
            "header\nfn main() {   \n    work();\t\n}\nfooter\n",
        )
        .unwrap();
        let engine = QuickJsEngine::new();
        let caps = Capabilities::read_write(&dir);

        // Leading whitespace stays significant in the fallback: only trailing
        // ASCII spaces and tabs are ignored.
        let leading = engine
            .run(
                r#"tools.replace("fallback.txt", "fn main() {\nwork();\n}", "x");"#,
                &caps,
            )
            .expect_err("removing leading whitespace must not match");
        assert!(
            leading.contains("matched 0 occurrences"),
            "unexpected leading-whitespace error: {leading}"
        );

        engine
            .run(
                r#"tools.replace("fallback.txt", "fn main() {\n    work();\n}", "fn main() {\n    done();\n}");"#,
                &caps,
            )
            .expect("trailing space and tab differences must match through the fallback");

        assert_eq!(
            fs::read_to_string(dir.join("fallback.txt")).unwrap(),
            "header\nfn main() {\n    done();\n}\nfooter\n"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn when_replacing_text_given_exact_tier_match_then_skips_whitespace_fallback() {
        let dir = scratch_dir("replace-tier-precedence");
        fs::write(dir.join("tier.txt"), "alpha \nalpha\n").unwrap();
        let engine = QuickJsEngine::new();
        let caps = Capabilities::read_write(&dir);

        // The exact tier finds the only "alpha " occurrence. The whitespace
        // fallback would find two "alpha" lines and reject the edit, so success
        // proves the fallback never ran.
        engine
            .run(r#"tools.replace("tier.txt", "alpha ", "beta");"#, &caps)
            .expect("the exact tier must take precedence over the whitespace fallback");

        assert_eq!(
            fs::read_to_string(dir.join("tier.txt")).unwrap(),
            "beta\nalpha\n"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn when_replacing_text_given_successful_edit_then_preserves_untouched_bytes_and_dominant_line_endings()
     {
        let dir = scratch_dir("replace-endings");
        let engine = QuickJsEngine::new();
        let caps = Capabilities::read_write(&dir);

        // Mixed endings where CRLF outnumbers bare LF: replacement CRLF is kept
        // and replacement LFs become CRLF; untouched regions keep their bytes.
        fs::write(dir.join("mixed.txt"), "a\r\nb\r\nc\n").unwrap();
        engine
            .run(r#"tools.replace("mixed.txt", "b", "B1\r\nB2");"#, &caps)
            .expect("mixed-ending replacement should succeed");
        assert_eq!(
            fs::read_to_string(dir.join("mixed.txt")).unwrap(),
            "a\r\nB1\r\nB2\r\nc\n"
        );

        // A tie between CRLF and bare LF resolves to LF.
        fs::write(dir.join("tie.txt"), "first\r\nsecond\n").unwrap();
        engine
            .run(
                r#"tools.replace("tie.txt", "first", "FIRST\nSECOND");"#,
                &caps,
            )
            .expect("tie replacement should succeed");
        assert_eq!(
            fs::read_to_string(dir.join("tie.txt")).unwrap(),
            "FIRST\nSECOND\r\nsecond\n"
        );

        // A replacement carrying CRLF is normalized to the LF-dominant file.
        fs::write(dir.join("lf.txt"), "one\ntwo\n").unwrap();
        engine
            .run(r#"tools.replace("lf.txt", "one", "ONE\r\nTWO");"#, &caps)
            .expect("LF-dominant replacement should succeed");
        assert_eq!(
            fs::read_to_string(dir.join("lf.txt")).unwrap(),
            "ONE\nTWO\ntwo\n"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn when_replacing_text_given_read_only_capability_or_confined_path_then_rejects_mutation() {
        let dir = scratch_dir("replace-access");
        fs::write(dir.join("note.txt"), "original\n").unwrap();
        let engine = QuickJsEngine::new();

        let read_only = Capabilities::read_only(&dir);
        let denied = engine
            .run(
                r#"tools.replace("note.txt", "original", "mutated");"#,
                &read_only,
            )
            .expect_err("read-only capabilities must not expose replace");
        assert!(
            denied.starts_with("[ERROR: execute_script failed]"),
            "unexpected read-only error prefix: {denied}"
        );

        let read_write = Capabilities::read_write(&dir);
        let escaped = engine
            .run(
                r#"tools.replace("../outside.txt", "original", "mutated");"#,
                &read_write,
            )
            .expect_err("a path outside the confined root must be rejected");
        assert!(
            escaped.contains("contains `..`"),
            "unexpected confinement error: {escaped}"
        );

        assert_eq!(
            fs::read_to_string(dir.join("note.txt")).unwrap(),
            "original\n"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn design_phases_expose_only_read_only_bindings() {
        let dir = scratch_dir("readonly-phase");
        fs::write(dir.join("note.txt"), "original").unwrap();
        let engine = QuickJsEngine::new();

        for phase in [
            Phase::Plan,
            Phase::Spec,
            Phase::Tickets,
            Phase::Triage,
            Phase::Review,
            Phase::Summary,
        ] {
            let caps = Capabilities::for_phase(phase).with_root(&dir);
            let keys = engine
                .run("return Object.keys(tools).sort();", &caps)
                .expect("keys query should succeed");
            assert_eq!(keys, r#"["diff","grep","list","read"]"#, "phase {phase:?}");

            let write_err = engine
                .run(r#"tools.write("note.txt", "mutated");"#, &caps)
                .expect_err("write must fail in read-only phase");
            assert!(write_err.starts_with("[ERROR: execute_script failed]"));
            assert_eq!(
                fs::read_to_string(dir.join("note.txt")).unwrap(),
                "original"
            );
        }

        let code_caps = Capabilities::for_phase(Phase::Code).with_root(&dir);
        let code_keys = engine
            .run("return Object.keys(tools).sort();", &code_caps)
            .expect("keys query should succeed");
        assert_eq!(
            code_keys,
            r#"["diff","grep","list","read","replace","write"]"#
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn path_boundary_rejects_parent_traversal_absolute_paths_and_symlink_escapes() {
        let dir = scratch_dir("boundary");
        let outside = scratch_dir("outside");
        let outside_file = outside.join("secret.txt");
        fs::write(&outside_file, "secret").unwrap();

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside_file, dir.join("escape_link")).unwrap();
            std::os::unix::fs::symlink(&outside, dir.join("escape_dir")).unwrap();
        }

        let engine = QuickJsEngine::new();
        let caps = Capabilities::read_write(&dir);

        for bad_path in ["../secret.txt", "a/../../secret.txt", "/etc/hosts"] {
            let err = engine
                .run(&format!("tools.read({bad_path:?});"), &caps)
                .expect_err("traversal or absolute path must be rejected");
            assert!(
                err.starts_with("[ERROR: execute_script failed]"),
                "path {bad_path}: {err}"
            );
        }

        #[cfg(unix)]
        {
            let link_err = engine
                .run(r#"tools.read("escape_link");"#, &caps)
                .expect_err("symlink file escape must be rejected");
            assert!(link_err.contains("escapes the project root"), "{link_err}");

            let dir_err = engine
                .run(r#"tools.write("escape_dir/pwned.txt", "no");"#, &caps)
                .expect_err("symlink dir escape must be rejected");
            assert!(dir_err.contains("escapes the project root"), "{dir_err}");
            assert!(!outside.join("pwned.txt").exists());
        }

        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&outside);
    }

    #[test]
    fn when_grep_given_ordinary_globs_ignored_files_and_no_matches_then_preserves_existing_results()
    {
        let dir = scratch_dir("git-bindings");
        init_git_repo(&dir);

        fs::write(dir.join(".gitignore"), "ignored.txt\n").unwrap();
        fs::write(dir.join("ignored.txt"), "findme in ignored\n").unwrap();
        fs::write(dir.join("a.rs"), "fn main() {\n    // findme in rust\n}\n").unwrap();
        fs::write(dir.join("b.md"), "# findme in docs\n").unwrap();

        let engine = QuickJsEngine::new();
        let caps = Capabilities::read_only(&dir);

        let all_files = engine
            .run("return tools.list().sort();", &caps)
            .expect("tools.list should succeed");
        assert_eq!(all_files, r#"[".gitignore","a.rs","b.md"]"#);

        let rs_files = engine
            .run(r#"return tools.list("*.rs");"#, &caps)
            .expect("tools.list with glob should succeed");
        assert_eq!(rs_files, r#"["a.rs"]"#);

        let grep_all = engine
            .run(r#"return tools.grep("findme");"#, &caps)
            .expect("tools.grep should succeed");
        let parsed: serde_json::Value = serde_json::from_str(&grep_all).unwrap();
        assert_eq!(
            parsed,
            serde_json::json!([
                { "path": "a.rs", "line": 2, "text": "    // findme in rust" },
                { "path": "b.md", "line": 1, "text": "# findme in docs" },
            ])
        );

        let grep_rs = engine
            .run(r#"return tools.grep("findme", "*.rs");"#, &caps)
            .expect("tools.grep with glob should succeed");
        let parsed_rs: serde_json::Value = serde_json::from_str(&grep_rs).unwrap();
        assert_eq!(
            parsed_rs,
            serde_json::json!([
                { "path": "a.rs", "line": 2, "text": "    // findme in rust" },
            ])
        );

        let grep_none = engine
            .run(r#"return tools.grep("definitely-not-present");"#, &caps)
            .expect("tools.grep with no match should return empty array");
        assert_eq!(grep_none, "[]");

        let _ = fs::remove_dir_all(&dir);
    }

    /// Maximum-limit boundary: more matching lines than the public cap still
    /// yield exactly the first `GREP_MATCH_LIMIT` records in Git order, each
    /// keeping the `{ path, line, text }` shape, while an empty pattern and a
    /// parent-traversal glob remain errors.
    #[test]
    fn when_grepping_files_given_more_than_one_thousand_matches_then_returns_first_one_thousand() {
        assert_eq!(
            GREP_MATCH_LIMIT, 1_000,
            "the public match cap must be 1,000"
        );

        let dir = scratch_dir("grep-match-limit");
        init_git_repo(&dir);

        let total = GREP_MATCH_LIMIT + 1;
        let body: String = (1..=total)
            .map(|line| format!("needle line {line}\n"))
            .collect();
        fs::write(dir.join("many.txt"), body).unwrap();

        let engine = QuickJsEngine::new();
        let caps = Capabilities::read_only(&dir);

        let summary = engine
            .run(
                r#"
                const matches = tools.grep("needle");
                const last = matches[matches.length - 1];
                return {
                    count: matches.length,
                    firstPath: matches[0].path,
                    firstLine: matches[0].line,
                    firstText: matches[0].text,
                    lastPath: last.path,
                    lastLine: last.line,
                    lastText: last.text,
                    beyondCap: matches.filter((match) => match.line === 1001).length,
                };
                "#,
                &caps,
            )
            .expect("tools.grep over the limit should succeed");
        let parsed: serde_json::Value = serde_json::from_str(&summary).unwrap();

        assert_eq!(
            parsed["count"],
            serde_json::json!(GREP_MATCH_LIMIT),
            "the result count must be capped: {summary}"
        );
        assert!(
            parsed["count"].as_u64().unwrap() <= GREP_MATCH_LIMIT as u64,
            "the result count must never exceed the cap: {summary}"
        );
        assert_eq!(parsed["firstPath"], serde_json::json!("many.txt"));
        assert_eq!(parsed["firstLine"], serde_json::json!(1));
        assert_eq!(parsed["firstText"], serde_json::json!("needle line 1"));
        assert_eq!(parsed["lastPath"], serde_json::json!("many.txt"));
        assert_eq!(parsed["lastLine"], serde_json::json!(1_000));
        assert_eq!(parsed["lastText"], serde_json::json!("needle line 1000"));
        assert_eq!(
            parsed["beyondCap"],
            serde_json::json!(0),
            "the line past the cap must not be returned: {summary}"
        );

        let empty = engine
            .run(r#"return tools.grep("");"#, &caps)
            .expect_err("an empty pattern must stay an error");
        assert!(
            empty.contains("`pattern` must not be empty"),
            "the existing empty-pattern error must be preserved: {empty}"
        );

        let traversal = engine
            .run(r#"return tools.grep("needle", "../outside/*.txt");"#, &caps)
            .expect_err("a parent-traversal glob must stay an error");
        assert!(
            traversal.contains("may not reference parent or root directories"),
            "the existing traversal glob error must be preserved: {traversal}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// Core entry-point behavior: a matched line longer than the public line
    /// cap is retained as the longest UTF-8 prefix that fits and carries the
    /// exact additive omission marker, while `path` and `line` are
    /// preserved. The multi-byte line stops on a character boundary even
    /// though a three-byte character crosses the 2,000-byte limit.
    #[test]
    fn when_grepping_files_given_line_over_two_thousand_bytes_then_truncates_matched_line_on_utf8_boundary()
     {
        assert_eq!(
            GREP_LINE_MAX_BYTES, 2_000,
            "the public per-line cap must be 2,000 bytes"
        );

        let dir = scratch_dir("grep-line-cap-over");
        init_git_repo(&dir);

        let ascii_line = format!("needle {}", "a".repeat(3_000));
        assert_eq!(ascii_line.len(), 3_007, "the ASCII fixture is over the cap");
        fs::write(dir.join("ascii.txt"), format!("{ascii_line}\n")).unwrap();

        let multibyte_prefix = format!("needle {}", "b".repeat(1_992));
        assert_eq!(multibyte_prefix.len(), 1_999);
        let multibyte_line = format!("{multibyte_prefix}€{}", "c".repeat(100));
        assert_eq!(multibyte_line.len(), 2_102);
        fs::write(dir.join("unicode.txt"), format!("{multibyte_line}\n")).unwrap();

        let engine = QuickJsEngine::new();
        let caps = Capabilities::read_only(&dir);

        let ascii = engine
            .run(r#"return tools.grep("needle", "ascii.txt");"#, &caps)
            .expect("tools.grep should succeed");
        let parsed: serde_json::Value = serde_json::from_str(&ascii).unwrap();
        let ascii_prefix = format!("needle {}", "a".repeat(1_993));
        let expected_ascii =
            format!("{ascii_prefix}... [line truncated to 2000 bytes; +1007 bytes]");
        assert_eq!(
            parsed,
            serde_json::json!([
                { "path": "ascii.txt", "line": 1, "text": expected_ascii },
            ]),
            "an over-limit ASCII line keeps its path and line and gains the exact marker: {ascii}"
        );

        let unicode = engine
            .run(r#"return tools.grep("needle", "unicode.txt");"#, &caps)
            .expect("tools.grep should succeed");
        let parsed_unicode: serde_json::Value = serde_json::from_str(&unicode).unwrap();
        let expected_unicode =
            format!("{multibyte_prefix}... [line truncated to 2000 bytes; +103 bytes]");
        assert_eq!(
            parsed_unicode,
            serde_json::json!([
                { "path": "unicode.txt", "line": 1, "text": expected_unicode },
            ]),
            "a multi-byte line retains a valid prefix of 1,999 bytes: {unicode}"
        );
        assert_eq!(
            parsed_unicode[0]["text"].as_str().unwrap().len(),
            expected_unicode.len(),
            "the retained prefix stops before the character crossing the limit"
        );
        assert_eq!(
            expected_unicode.len(),
            1_999 + "... [line truncated to 2000 bytes; +103 bytes]".len()
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// Empty/zero and malformed boundaries: a search with no match is still an
    /// empty array, and an empty pattern still fails with the established
    /// error, even when a run artifact contains the pattern.
    #[test]
    fn when_grepping_files_given_no_matches_or_empty_pattern_then_preserves_existing_results_and_error()
     {
        let dir = scratch_dir("grep-no-match-empty-pattern");
        init_git_repo(&dir);
        fs::write(dir.join("ordinary.md"), "# nothing to find here\n").unwrap();
        fs::create_dir_all(dir.join(".ejaj/run")).unwrap();
        fs::write(
            dir.join(".ejaj/run/handoff.md"),
            "needle only in artifact\n",
        )
        .unwrap();

        let engine = QuickJsEngine::new();
        let caps = Capabilities::read_only(&dir);

        let no_matches = engine
            .run(r#"return tools.grep("needle");"#, &caps)
            .expect("a no-match search is an empty array");
        assert_eq!(no_matches, "[]");

        let empty = engine
            .run(r#"return tools.grep("");"#, &caps)
            .expect_err("an empty pattern stays an error");
        assert!(
            empty.starts_with("[ERROR: execute_script failed]"),
            "the error keeps its established shape: {empty}"
        );
        assert!(
            empty.contains("`pattern` must not be empty"),
            "the empty-pattern error is unchanged: {empty}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// Maximum boundary: a matched line of exactly 2,000 UTF-8 bytes is
    /// returned unchanged and carries no truncation marker.
    #[test]
    fn when_grepping_files_given_line_exactly_two_thousand_bytes_then_returns_unmarked_text() {
        let dir = scratch_dir("grep-line-cap-exact");
        init_git_repo(&dir);
        let exactly = format!("needle {}", "e".repeat(1_993));
        assert_eq!(exactly.len(), 2_000);
        fs::write(dir.join("exact.txt"), format!("{exactly}\n")).unwrap();

        let engine = QuickJsEngine::new();
        let caps = Capabilities::read_only(&dir);

        let out = engine
            .run(r#"return tools.grep("needle");"#, &caps)
            .expect("tools.grep should succeed");
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            parsed,
            serde_json::json!([
                { "path": "exact.txt", "line": 1, "text": exactly },
            ])
        );
        assert!(
            !parsed[0]["text"].as_str().unwrap().contains("truncated"),
            "a line at the cap is not marked: {out}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// Contract/invariant: each returned record is capped on its own, in the
    /// exact Git order, while the `{ path, line, text }` fields stay stable.
    #[test]
    fn when_grepping_files_given_multiple_long_matches_then_caps_each_line_and_preserves_git_order()
    {
        let dir = scratch_dir("grep-line-cap-multiple");
        init_git_repo(&dir);

        let first = format!("needle {}", "a".repeat(2_493));
        assert_eq!(first.len(), 2_500);
        let second = format!("needle {}", "b".repeat(2_993));
        assert_eq!(second.len(), 3_000);
        fs::write(dir.join("a.txt"), format!("{first}\n{second}\n")).unwrap();

        let exact = format!("needle {}", "c".repeat(1_993));
        assert_eq!(exact.len(), 2_000);
        fs::write(dir.join("b.txt"), format!("{exact}\nneedle short\n")).unwrap();

        let engine = QuickJsEngine::new();
        let caps = Capabilities::read_only(&dir);

        let out = engine
            .run(r#"return tools.grep("needle");"#, &caps)
            .expect("tools.grep should succeed");
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        let first_prefix = format!("needle {}", "a".repeat(1_993));
        let expected_first =
            format!("{first_prefix}... [line truncated to 2000 bytes; +500 bytes]");
        let second_prefix = format!("needle {}", "b".repeat(1_993));
        let expected_second =
            format!("{second_prefix}... [line truncated to 2000 bytes; +1000 bytes]");
        assert_eq!(
            parsed,
            serde_json::json!([
                { "path": "a.txt", "line": 1, "text": expected_first },
                { "path": "a.txt", "line": 2, "text": expected_second },
                { "path": "b.txt", "line": 1, "text": exact },
                { "path": "b.txt", "line": 2, "text": "needle short" },
            ]),
            "each line is capped independently in Git order: {out}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// Malformed/invalid payload boundary: traversal and absolute globs are
    /// rejected before `git grep` runs.
    #[test]
    fn when_grepping_files_given_invalid_glob_then_rejects_traversal() {
        let dir = scratch_dir("grep-invalid-glob");
        init_git_repo(&dir);
        fs::write(dir.join("ordinary.txt"), "needle here\n").unwrap();

        let engine = QuickJsEngine::new();
        let caps = Capabilities::read_only(&dir);

        let traversal = engine
            .run(r#"return tools.grep("needle", "../outside/*.txt");"#, &caps)
            .expect_err("a parent-traversal glob must be rejected");
        assert!(
            traversal.starts_with("[ERROR: execute_script failed]"),
            "unexpected error shape: {traversal}"
        );
        assert!(
            traversal.contains("may not reference parent or root directories"),
            "the traversal glob must name its rejection: {traversal}"
        );

        let absolute = engine
            .run(r#"return tools.grep("needle", "/etc/*.txt");"#, &caps)
            .expect_err("an absolute glob must be rejected");
        assert!(
            absolute.contains("must be relative to the project root"),
            "the absolute glob must name its rejection: {absolute}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// Regression: the existing grep contracts survive the line cap -- empty
    /// pattern errors, null and omitted globs search the same tree, glob
    /// validation still rejects traversal, `.ejaj/` artifacts stay filtered,
    /// and the result count remains capped at 1,000.
    #[test]
    fn when_grepping_files_given_empty_pattern_null_or_omitted_glob_and_path_filters_then_preserves_existing_contracts()
     {
        let dir = scratch_dir("grep-existing-contracts");
        init_git_repo(&dir);
        fs::write(dir.join("a.rs"), "fn main() {}\n// findme in rust\n").unwrap();
        fs::write(dir.join("b.md"), "# findme in docs\n").unwrap();
        fs::create_dir_all(dir.join(".ejaj/run")).unwrap();
        fs::write(dir.join(".ejaj/run/handoff.md"), "findme in artifact\n").unwrap();
        let many: String = (1..=1_001)
            .map(|line| format!("needle line {line}\n"))
            .collect();
        fs::write(dir.join("many.txt"), many).unwrap();

        let engine = QuickJsEngine::new();
        let caps = Capabilities::read_only(&dir);

        let omitted = engine
            .run(r#"return tools.grep("findme");"#, &caps)
            .expect("tools.grep without a glob should succeed");
        let parsed_omitted: serde_json::Value = serde_json::from_str(&omitted).unwrap();
        assert_eq!(
            parsed_omitted,
            serde_json::json!([
                { "path": "a.rs", "line": 2, "text": "// findme in rust" },
                { "path": "b.md", "line": 1, "text": "# findme in docs" },
            ]),
            "run artifacts stay filtered from the whole-tree search: {omitted}"
        );

        let null_glob = engine
            .run(r#"return tools.grep("findme", null);"#, &caps)
            .expect("a null glob should behave like an omitted glob");
        assert_eq!(null_glob, omitted, "a null glob changes nothing");

        let narrowed = engine
            .run(r#"return tools.grep("findme", "*.rs");"#, &caps)
            .expect("an ordinary glob should narrow the search");
        let parsed_narrowed: serde_json::Value = serde_json::from_str(&narrowed).unwrap();
        assert_eq!(
            parsed_narrowed,
            serde_json::json!([
                { "path": "a.rs", "line": 2, "text": "// findme in rust" },
            ])
        );

        let artifact_glob = engine
            .run(r#"return tools.grep("findme", ".ejaj/**");"#, &caps)
            .expect("an explicit artifact glob should succeed");
        assert_eq!(artifact_glob, "[]", "an artifact glob reveals nothing");

        let empty = engine
            .run(r#"return tools.grep("");"#, &caps)
            .expect_err("an empty pattern stays an error");
        assert!(
            empty.contains("`pattern` must not be empty"),
            "the empty-pattern error is unchanged: {empty}"
        );

        let invalid = engine
            .run(r#"return tools.grep("findme", "../outside/*.txt");"#, &caps)
            .expect_err("a traversal glob stays an error");
        assert!(
            invalid.contains("may not reference parent or root directories"),
            "the glob-validation error is unchanged: {invalid}"
        );

        let limited = engine
            .run(
                r#"
                const matches = tools.grep("needle");
                return {
                    count: matches.length,
                    firstLine: matches[0].line,
                    firstText: matches[0].text,
                    lastLine: matches[matches.length - 1].line,
                };
                "#,
                &caps,
            )
            .expect("tools.grep over the limit should succeed");
        let parsed_limited: serde_json::Value = serde_json::from_str(&limited).unwrap();
        assert_eq!(parsed_limited["count"], serde_json::json!(1_000));
        assert_eq!(parsed_limited["firstLine"], serde_json::json!(1));
        assert_eq!(
            parsed_limited["firstText"],
            serde_json::json!("needle line 1")
        );
        assert_eq!(parsed_limited["lastLine"], serde_json::json!(1_000));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn output_is_truncated_at_the_cap_without_splitting_utf8() {
        let dir = scratch_dir("output-cap");
        let engine = QuickJsEngine::new().with_output_limit(512);
        let caps = Capabilities::read_only(&dir);

        let out = engine
            .run(r#"console.log("x".repeat(5000));"#, &caps)
            .expect("script should succeed");
        assert!(
            out.len() <= 512,
            "the complete output including its marker must stay within the cap: {}",
            out.len()
        );
        assert!(
            out.contains("[script output truncated to 512 bytes;"),
            "unexpected output: {out:?}"
        );
        assert!(
            out.contains("filter results in JavaScript"),
            "the marker must advise filtering in JavaScript: {out:?}"
        );

        let source = format!("{}€", "a".repeat(511));
        let utf8 = cap_script_output(source.clone(), 512);
        assert!(
            utf8.len() <= 512,
            "the marker must fit inside the cap: {}",
            utf8.len()
        );
        let marker_start = utf8
            .find("\n[script output truncated")
            .expect("the truncation marker is present");
        let kept = &utf8[..marker_start];
        let omitted = source.len() - kept.len();
        assert!(
            kept.chars().all(|c| c == 'a'),
            "the cut split a character: {kept:?}"
        );
        assert!(
            utf8[marker_start..].contains(&format!("{omitted} bytes omitted")),
            "{utf8:?}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// Regression: output exactly at the configured cap passes through the
    /// real script engine byte for byte, with no truncation marker.
    #[test]
    fn when_execute_script_given_output_at_limit_then_returns_unchanged_text() {
        let dir = scratch_dir("execute-script-at-limit");
        let engine = QuickJsEngine::new().with_output_limit(512);
        let caps = Capabilities::read_only(&dir);

        let out = engine
            .run(r#"console.log("a".repeat(512));"#, &caps)
            .expect("the script should succeed");

        assert_eq!(out, "a".repeat(512));
        assert!(
            !out.contains("truncated"),
            "output at the cap must not be truncated: {out:?}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// Minimum valid boundary: output exactly at the production cap is handed
    /// back unchanged by the real engine, with no truncation marker at all.
    #[test]
    fn when_execute_script_given_output_exactly_at_limit_then_returns_unchanged_text() {
        let dir = scratch_dir("execute-script-exactly-at-limit");
        let engine = QuickJsEngine::new();
        let caps = Capabilities::read_only(&dir);

        let out = engine
            .run(
                &format!(r#"return "a".repeat({OUTPUT_LIMIT_BYTES});"#),
                &caps,
            )
            .expect("the script should succeed");

        assert_eq!(out, "a".repeat(OUTPUT_LIMIT_BYTES));
        assert!(
            !out.contains("truncated"),
            "output exactly at the cap must not be truncated: {out:?}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// The upper boundary: one byte more than the cap is truncated, the marker
    /// reports the exact number of source bytes the model did not see, and the
    /// whole returned value -- marker included -- stays within the cap.
    #[test]
    fn when_script_output_given_over_limit_text_then_reports_exact_omitted_bytes_within_limit() {
        let text = "a".repeat(OUTPUT_LIMIT_BYTES + 100);
        let output = cap_script_output(text.clone(), OUTPUT_LIMIT_BYTES);

        assert!(
            output.len() <= OUTPUT_LIMIT_BYTES,
            "the returned script output is {} bytes, over the {OUTPUT_LIMIT_BYTES}-byte cap",
            output.len()
        );
        let marker_start = output
            .find("\n[script output truncated")
            .expect("the truncation marker is present");
        let kept = &output[..marker_start];
        let omitted = text.len() - kept.len();
        assert!(
            output[marker_start..].contains(&format!("{omitted} bytes omitted")),
            "the marker must report the exact {omitted} omitted source bytes: {}",
            &output[marker_start..]
        );
        assert!(
            output[marker_start..].contains("filter results in JavaScript"),
            "the marker must advise filtering in JavaScript: {}",
            &output[marker_start..]
        );
    }

    /// Maximum limit / upper boundary: multibyte output above the cap is cut on
    /// a character boundary, never splitting a euro sign or introducing a
    /// replacement character, and the marker reports the exact omitted source
    /// bytes together with the JavaScript-filtering advice.
    #[test]
    fn when_execute_script_given_multibyte_output_above_limit_then_truncates_on_character_boundary_with_exact_omitted_bytes()
     {
        let dir = scratch_dir("oversized-multibyte");
        let engine = QuickJsEngine::new();
        let caps = Capabilities::read_only(&dir);
        let source = "€".repeat(12000);

        let out = engine
            .run(r#"return "€".repeat(12000);"#, &caps)
            .expect("the script should succeed");

        assert!(
            out.len() <= OUTPUT_LIMIT_BYTES,
            "the returned script output is {} bytes, over the {OUTPUT_LIMIT_BYTES}-byte cap",
            out.len()
        );
        assert!(
            !out.contains('\u{FFFD}'),
            "a valid output path must not introduce a replacement character: {out:?}"
        );
        let marker_start = out
            .find("\n[script output truncated")
            .expect("the marker splits the content");
        let content = &out[..marker_start];
        let omitted = source.len() - content.len();
        assert!(
            content.chars().all(|c| c == '€'),
            "the retained content must stay on a UTF-8 character boundary: {content:?}"
        );
        assert!(
            out[marker_start..].contains(&format!("{omitted} bytes omitted")),
            "the marker must report the exact {omitted} omitted source bytes: {}",
            &out[marker_start..]
        );
        assert!(
            out[marker_start..].contains("filter results in JavaScript"),
            "the truncation marker must advise filtering in JavaScript: {out:?}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn source_limit_memory_limit_and_deadline_are_enforced() {
        let dir = scratch_dir("limits");
        let caps = Capabilities::read_only(&dir);

        let small_source_engine = QuickJsEngine::new().with_source_limit(16);
        let source_err = small_source_engine
            .run("const too_long_program = 1234567890;", &caps)
            .expect_err("oversized source must be rejected");
        assert!(source_err.contains("exceeds the 16-byte source limit"));

        let small_mem_engine = QuickJsEngine::new().with_memory_limit(2 * 1024 * 1024);
        let mem_err = small_mem_engine
            .run(
                "const big = 'x'.repeat(10 * 1024 * 1024); return big.length;",
                &caps,
            )
            .expect_err("memory limit breach must fail");
        assert!(
            mem_err.contains("memory limit"),
            "unexpected memory error: {mem_err}"
        );

        let short_deadline_engine = QuickJsEngine::new().with_deadline(Duration::from_millis(20));
        let timeout_err = short_deadline_engine
            .run("while (true) {}", &caps)
            .expect_err("infinite loop must be interrupted by deadline");
        assert!(
            timeout_err.contains("wall-clock deadline"),
            "unexpected timeout error: {timeout_err}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// The triage pass is read-only: its capabilities expose the four
    /// inspection bindings and no writable one, so a write or a replace is the
    /// forbidden boundary the pass actually hits -- and the file it names is
    /// left exactly as it was.
    #[test]
    fn when_inspecting_repository_given_triage_phase_then_rejects_write_and_replace() {
        let dir = scratch_dir("triage-readonly");
        fs::write(dir.join("note.txt"), "original").unwrap();
        let engine = QuickJsEngine::new();
        let caps = Capabilities::for_phase(Phase::Triage).with_root(&dir);

        let keys = engine
            .run("return Object.keys(tools).sort();", &caps)
            .expect("the keys query should succeed");
        assert_eq!(keys, r#"["diff","grep","list","read"]"#);
        assert!(!caps.writable, "the triage phase must be read-only");

        let inspection = engine
            .run("return tools.read(\"note.txt\");", &caps)
            .expect("the read-only bindings stay available");
        assert_eq!(inspection, "original");

        for code in [
            r#"tools.write("note.txt", "mutated");"#,
            r#"tools.replace("note.txt", "original", "mutated");"#,
        ] {
            let error = engine
                .run(code, &caps)
                .expect_err("a writable binding must be refused in the triage phase");
            assert!(
                error.starts_with("[ERROR: execute_script failed]"),
                "unexpected error shape: {error}"
            );
            assert!(
                error.contains("not a function"),
                "the binding must not exist at all: {error}"
            );
            assert_eq!(
                fs::read_to_string(dir.join("note.txt")).unwrap(),
                "original"
            );
        }

        let _ = fs::remove_dir_all(&dir);
    }

    // -- Discovery bindings: run artifacts stay invisible to list and grep

    /// Core entry-point behavior at the minimum valid boundary: with no caller
    /// glob, a repository that has no root ignore rule for `.ejaj/` still
    /// hides its run artifacts from listing and search, while ordinary tracked
    /// and untracked files remain visible and grep keeps its
    /// `{path, line, text}` shape.
    #[test]
    fn when_discovering_files_given_no_glob_then_returns_ordinary_files_and_matches() {
        let dir = scratch_dir("discovery-unignored-artifacts");
        init_git_repo(&dir);
        fs::write(dir.join("guide.rs"), "// findme in tracked rust\n").unwrap();
        run_git(&dir, &["add", "-A"]);
        run_git(&dir, &["commit", "-q", "-m", "initial"]);

        fs::write(dir.join("notes.md"), "# findme in untracked docs\n").unwrap();
        fs::create_dir_all(dir.join(".ejaj/run/tools-output")).unwrap();
        fs::write(dir.join(".ejaj/run/handoff.md"), "findme in handoff\n").unwrap();
        fs::write(
            dir.join(".ejaj/run/tools-output/step-1-call-1.log"),
            "findme in spill\n",
        )
        .unwrap();

        let engine = QuickJsEngine::new();
        let caps = Capabilities::read_only(&dir);

        let listed = engine
            .run("return tools.list().sort();", &caps)
            .expect("tools.list should succeed");
        assert_eq!(
            listed, r#"["guide.rs","notes.md"]"#,
            "ordinary tracked and untracked files must stay visible"
        );

        let grepped = engine
            .run(r#"return tools.grep("findme");"#, &caps)
            .expect("tools.grep should succeed");
        let parsed: serde_json::Value = serde_json::from_str(&grepped).unwrap();
        assert_eq!(
            parsed,
            serde_json::json!([
                { "path": "guide.rs", "line": 1, "text": "// findme in tracked rust" },
                { "path": "notes.md", "line": 1, "text": "# findme in untracked docs" },
            ]),
            "grep must omit run artifacts and keep its {{path, line, text}} shape: {grepped}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// Maximum limit / upper boundary: the deepest normal spill path remains
    /// directly readable through the read binding, byte for byte, even though
    /// discovery hides it.
    #[test]
    fn when_reading_spill_given_known_ejaj_path_then_returns_contents() {
        let dir = scratch_dir("read-spill-artifact");
        let spill = ".ejaj/2026-01-02T03-04-05/tools-output/step-12-call-3.log";
        fs::create_dir_all(dir.join(".ejaj/2026-01-02T03-04-05/tools-output")).unwrap();
        let contents = "raw stdout\nraw stderr\nno trailing newline";
        fs::write(dir.join(spill), contents).unwrap();

        let engine = QuickJsEngine::new();
        let caps = Capabilities::read_only(&dir);

        let read = engine
            .run(&format!("return tools.read({spill:?});"), &caps)
            .expect("a spill file inside the project root stays readable");
        assert_eq!(
            read, contents,
            "the spill contents must be returned exactly"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// Empty/zero boundary: a search whose pattern matches nothing is still an
    /// empty array, not an error and not an artifact-only result.
    #[test]
    fn when_searching_files_given_pattern_without_matches_then_returns_empty_array() {
        let dir = scratch_dir("grep-no-match");
        init_git_repo(&dir);
        fs::write(dir.join("ordinary.md"), "# nothing to find here\n").unwrap();
        fs::create_dir_all(dir.join(".ejaj/run")).unwrap();
        fs::write(
            dir.join(".ejaj/run/handoff.md"),
            "findme only in artifact\n",
        )
        .unwrap();

        let engine = QuickJsEngine::new();
        let caps = Capabilities::read_only(&dir);

        let out = engine
            .run(r#"return tools.grep("definitely-not-present");"#, &caps)
            .expect("tools.grep with no match should return an empty array");
        assert_eq!(out, "[]");

        let _ = fs::remove_dir_all(&dir);
    }

    /// Empty payload boundary: an empty pattern keeps the existing error.
    #[test]
    fn when_searching_files_given_empty_pattern_then_returns_existing_error() {
        let dir = scratch_dir("grep-empty-pattern");
        init_git_repo(&dir);
        fs::write(dir.join("ordinary.md"), "# content\n").unwrap();

        let engine = QuickJsEngine::new();
        let caps = Capabilities::read_only(&dir);

        let error = engine
            .run(r#"return tools.grep("");"#, &caps)
            .expect_err("an empty pattern must fail");
        assert!(
            error.starts_with("[ERROR: execute_script failed]"),
            "unexpected error shape: {error}"
        );
        assert!(
            error.contains("`pattern` must not be empty"),
            "the existing empty-pattern error must be preserved: {error}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// Core entry-point behavior: a colon in the path and in the matched text
    /// is never a field separator, so an ordinary search returns the exact
    /// `{ path, line, text }` record for the one matching line.
    #[test]
    fn when_searching_files_given_colon_in_path_then_returns_exact_match() {
        let dir = scratch_dir("grep-colon-path");
        init_git_repo(&dir);
        fs::create_dir_all(dir.join("subdir")).unwrap();
        fs::write(
            dir.join("subdir/with:colon.txt"),
            "ignored\nneedle: matched text\n",
        )
        .unwrap();

        let engine = QuickJsEngine::new();
        let caps = Capabilities::read_only(&dir);

        let out = engine
            .run(r#"return tools.grep("needle");"#, &caps)
            .expect("tools.grep should succeed");
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            parsed,
            serde_json::json!([
                { "path": "subdir/with:colon.txt", "line": 2, "text": "needle: matched text" },
            ]),
            "colons in the path or text must not be field separators: {out}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// Malformed/invalid payload boundary: an explicit glob that names
    /// `.ejaj/` cannot reveal artifacts, while an ordinary glob keeps working.
    #[test]
    fn when_searching_files_given_explicit_ejaj_glob_then_omits_artifacts() {
        let dir = scratch_dir("grep-artifact-glob");
        init_git_repo(&dir);
        fs::create_dir_all(dir.join(".ejaj/run/tools-output")).unwrap();
        fs::write(dir.join(".ejaj/run/handoff.md"), "findme in handoff\n").unwrap();
        fs::write(
            dir.join(".ejaj/run/tools-output/step-1-call-1.log"),
            "findme in spill\n",
        )
        .unwrap();
        fs::write(dir.join("ordinary.md"), "# findme ordinary\n").unwrap();

        let engine = QuickJsEngine::new();
        let caps = Capabilities::read_only(&dir);

        for glob in [
            ".ejaj",
            ".ejaj/**",
            ".ejaj/*",
            ".ejaj/**/*.md",
            ".ejaj/**/*.log",
        ] {
            let out = engine
                .run(&format!(r#"return tools.grep("findme", {glob:?});"#), &caps)
                .expect("grep with an explicit artifact glob should succeed");
            assert_eq!(out, "[]", "the glob `{glob}` must not reveal run artifacts");
        }

        let ordinary = engine
            .run(r#"return tools.grep("findme", "*.md");"#, &caps)
            .expect("an ordinary glob should keep working");
        let parsed: serde_json::Value = serde_json::from_str(&ordinary).unwrap();
        assert_eq!(
            parsed,
            serde_json::json!([
                { "path": "ordinary.md", "line": 1, "text": "# findme ordinary" },
            ])
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// Malformed/invalid payload boundary: an explicit glob that names
    /// `.ejaj/` cannot reveal artifacts through listing, while an ordinary glob
    /// keeps working.
    #[test]
    fn when_listing_files_given_explicit_ejaj_glob_then_omits_artifacts() {
        let dir = scratch_dir("list-artifact-glob");
        init_git_repo(&dir);
        fs::create_dir_all(dir.join(".ejaj/run/tools-output")).unwrap();
        fs::write(dir.join(".ejaj/run/handoff.md"), "handoff\n").unwrap();
        fs::write(
            dir.join(".ejaj/run/tools-output/step-1-call-1.log"),
            "spill\n",
        )
        .unwrap();
        fs::write(dir.join("ordinary.md"), "# ordinary\n").unwrap();

        let engine = QuickJsEngine::new();
        let caps = Capabilities::read_only(&dir);

        for glob in [".ejaj", ".ejaj/**", ".ejaj/*", ".ejaj/**/*.log"] {
            let out = engine
                .run(&format!("return tools.list({glob:?});"), &caps)
                .expect("listing with an explicit artifact glob should succeed");
            assert_eq!(out, "[]", "the glob `{glob}` must not reveal run artifacts");
        }

        let ordinary = engine
            .run(r#"return tools.list("*.md");"#, &caps)
            .expect("an ordinary glob should keep working");
        assert_eq!(ordinary, r#"["ordinary.md"]"#);

        let _ = fs::remove_dir_all(&dir);
    }

    /// Contract and invariant check at the minimum valid boundary: the filter
    /// covers tracked artifacts as well as untracked ones, without relying on
    /// any ignore rule, and only ordinary files are returned.
    #[test]
    fn when_discovering_files_given_tracked_and_untracked_ejaj_paths_then_returns_only_ordinary_files()
     {
        let dir = scratch_dir("discovery-tracked-artifacts");
        init_git_repo(&dir);
        fs::create_dir_all(dir.join(".ejaj/run")).unwrap();
        fs::write(
            dir.join(".ejaj/run/handoff.md"),
            "findme tracked artifact\n",
        )
        .unwrap();
        fs::write(dir.join("tracked.md"), "findme tracked ordinary\n").unwrap();
        run_git(&dir, &["add", "-A"]);
        run_git(&dir, &["commit", "-q", "-m", "initial"]);

        let tracked = Command::new("git")
            .current_dir(&dir)
            .args(["ls-files"])
            .output()
            .expect("git is installed and runs");
        assert!(tracked.status.success(), "`git ls-files` failed");
        assert!(
            String::from_utf8_lossy(&tracked.stdout).contains(".ejaj/run/handoff.md"),
            "the artifact must be tracked for this test to prove the filter"
        );

        fs::write(
            dir.join(".ejaj/run/spill.log"),
            "findme untracked artifact\n",
        )
        .unwrap();
        fs::write(dir.join("untracked.md"), "findme untracked ordinary\n").unwrap();

        let engine = QuickJsEngine::new();
        let caps = Capabilities::read_only(&dir);

        let listed = engine
            .run("return tools.list().sort();", &caps)
            .expect("tools.list should succeed");
        assert_eq!(listed, r#"["tracked.md","untracked.md"]"#);

        let grepped = engine
            .run(r#"return tools.grep("findme");"#, &caps)
            .expect("tools.grep should succeed");
        let parsed: serde_json::Value = serde_json::from_str(&grepped).unwrap();
        assert_eq!(
            parsed,
            serde_json::json!([
                { "path": "tracked.md", "line": 1, "text": "findme tracked ordinary" },
                { "path": "untracked.md", "line": 1, "text": "findme untracked ordinary" },
            ])
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// Unauthorized/forbidden boundary: a read that leaves the project root is
    /// still rejected, while the in-root spill read above stays direct.
    #[test]
    fn when_reading_files_given_path_outside_project_root_then_rejects_access() {
        let dir = scratch_dir("read-outside-root");
        let outside = scratch_dir("read-outside-root-secret");
        fs::write(outside.join("secret.txt"), "secret\n").unwrap();

        let engine = QuickJsEngine::new();
        let caps = Capabilities::read_only(&dir);

        for bad_path in ["../secret.txt", "nested/../../secret.txt"] {
            let error = engine
                .run(&format!("tools.read({bad_path:?});"), &caps)
                .expect_err("a parent traversal must be rejected");
            assert!(
                error.starts_with("[ERROR: execute_script failed]"),
                "path {bad_path}: {error}"
            );
            assert!(
                error.contains(".."),
                "the error must name the traversal for {bad_path}: {error}"
            );
        }

        let absolute = outside.join("secret.txt");
        let absolute = absolute.to_string_lossy().to_string();
        let error = engine
            .run(&format!("tools.read({absolute:?});"), &caps)
            .expect_err("an absolute path must be rejected");
        assert!(
            error.contains("must be relative to the project root"),
            "unexpected absolute-path error: {error}"
        );

        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&outside);
    }

    /// Regression at the empty/zero boundary: ordinary list and grep globs keep
    /// their existing results and public result shapes.
    #[test]
    fn when_discovering_files_given_ordinary_globs_then_preserves_existing_results() {
        let dir = scratch_dir("discovery-ordinary-globs");
        init_git_repo(&dir);
        fs::write(dir.join("a.rs"), "fn main() {\n    // findme in rust\n}\n").unwrap();
        fs::write(dir.join("b.md"), "# findme in docs\n").unwrap();
        fs::write(dir.join("notes.txt"), "findme in notes\n").unwrap();

        let engine = QuickJsEngine::new();
        let caps = Capabilities::read_only(&dir);

        let listed_rs = engine
            .run(r#"return tools.list("*.rs");"#, &caps)
            .expect("tools.list with a glob should succeed");
        assert_eq!(listed_rs, r#"["a.rs"]"#);

        let listed_md = engine
            .run(r#"return tools.list("*.md");"#, &caps)
            .expect("tools.list with a glob should succeed");
        assert_eq!(listed_md, r#"["b.md"]"#);

        let grepped_rs = engine
            .run(r#"return tools.grep("findme", "*.rs");"#, &caps)
            .expect("tools.grep with a glob should succeed");
        let parsed_rs: serde_json::Value = serde_json::from_str(&grepped_rs).unwrap();
        assert_eq!(
            parsed_rs,
            serde_json::json!([
                { "path": "a.rs", "line": 2, "text": "    // findme in rust" },
            ])
        );

        let grepped_all = engine
            .run(r#"return tools.grep("findme");"#, &caps)
            .expect("tools.grep should succeed");
        let parsed_all: serde_json::Value = serde_json::from_str(&grepped_all).unwrap();
        assert_eq!(
            parsed_all,
            serde_json::json!([
                { "path": "a.rs", "line": 2, "text": "    // findme in rust" },
                { "path": "b.md", "line": 1, "text": "# findme in docs" },
                { "path": "notes.txt", "line": 1, "text": "findme in notes" },
            ])
        );

        let _ = fs::remove_dir_all(&dir);
    }
}
