//! The "the agent is working" status line, and the transcript's line markers.
//!
//! Two markers keep the transcript readable at a glance: a tool call
//! ([`call_line`]) carries its step counter and a `$ ` marker before the
//! command display, and `# ` heads a line the model itself wrote when the
//! transcript is redirected (`plain_agent_text`). Those two markers are plain
//! text, so a piped or redirected transcript carries exactly the same marks as
//! an interactive one. Styling is layered on top of a marker, never in place of
//! one: on a terminal the whole call line is painted in the palette's cyan
//! ([`render_tool_call`], applied by the [`OutputCoordinator`]) and narration is
//! rendered as Markdown rather than as a column of literal `#` markers
//! (`agent_text`), so the model's headings, emphasis and lists arrive styled --
//! in the Tokyo Night-inspired palette of `tokyo_night_skin`. A redirected or
//! piped stdout stays plain on both counts: the marker is always there, the
//! escape codes are not.
//!
//! Log severity is the third, smallest style: a log line that begins with the
//! exact `[INFO]`, `[WARN]` or `[ERROR]` has only that marker painted -- in the
//! conventional cyan, yellow or red ([`render_log`]) -- on whichever stream
//! carries it, and every other byte of the line is reproduced as written. An
//! unrecognized, differently cased or non-leading tag is left alone, so a
//! redirected or piped log stays plain.
//!
//! The whole program is synchronous: `run_agent_loop` blocks inside a plain
//! `minreq::post(..).send()`, and tool output is only printed once the command
//! has finished. Without something on screen, a slow call and a crashed one
//! look identical -- which is exactly how a reasoning-only reply used to
//! vanish (empty `content`, nothing printed, prompt straight back).
//!
//! So: the label is drawn as the *last line of the transcript* and erased
//! right before any real output lands. There is no animation, no background
//! thread and no absolute cursor positioning -- no escape sequence ever moves
//! the cursor backwards, so nothing can be clobbered and a kill mid-call
//! leaves nothing worse than a stray line of text. Erasing uses `\r\x1b[K`,
//! which clears only the current row, hence `MAX_LABEL`: a label that wrapped
//! would leave a partial row of garbage behind.
//!
//! The thinking label ([`thinking_label`]) names the canonical phase of the
//! exact request it describes -- [`Phase::label`] followed by `...`, so
//! `code...` or `summary...` -- then the provider role serving it, then
//! that request's encoded context size as two grouped figures. The role is one
//! of the three canonical lowercase provider-role names, `mid`, `giga` or
//! `smoll`, rendered in brackets right after the phase prefix. The first
//! figure is plain bytes: no unit scaling (KiB/MiB) and no rounding. The second
//! is an *approximate* token count -- the byte figure floor-divided by
//! `BYTES_PER_TOKEN`, a fixed bytes-per-token heuristic rather than a tokenizer
//! count. A caller that has the selected provider's optional per-million input
//! price passes it as the label's optional rate: a configured rate appends an
//! input-only USD estimate calculated from that same displayed integer token
//! count, while a missing rate keeps the prior, unpriced shape. The composed
//! label is passed through `truncate_row` and capped at `MAX_LABEL`
//! characters, so an absurd context can have a figure cut in half, even though
//! the label still cannot wrap.
//!
//! That label machinery -- and its non-terminal no-op -- applies only to the
//! one-row status and call labels. Narration has its own, separate switch:
//! [`agent_text`] asks `IsTerminal` about *stdout* directly, not the status
//! line's `TERM=dumb` check, and on a terminal renders the model's Markdown
//! with the module's one canonical Tokyo Night skin (`tokyo_night_skin`),
//! while every redirected transcript keeps the `# `-prefixed plain text.
//! [`render_agent_text`] is the same decision with the answer passed in, so
//! unit tests need neither a terminal nor any mutation of the real stdout.
//! Non-terminal (and dumb) stdout still disables the status line, so piped
//! input and redirected logs stay free of escape codes; in that mode every
//! `Status` method is a no-op and no call site has to care.

use std::env;
use std::io::{self, IsTerminal, Write};
use std::time::Duration;

use termimad::MadSkin;
use termimad::crossterm::Command;
use termimad::crossterm::style::{Attribute, Color, ResetColor, SetForegroundColor};

use crate::tools::Phase;

/// The floor-division divisor behind the approximate token figure in
/// [`thinking_label`]: roughly four bytes of context per token. It is a fixed heuristic, not read from the
/// environment, and not part of `Config`.
const BYTES_PER_TOKEN: usize = 4;

/// The denominator of a configured per-million input price. The estimate is
/// calculated from the displayed integer token count, never from raw bytes or
/// provider-reported usage.
const TOKENS_PER_MILLION: f64 = 1_000_000.0;

/// The status label for one in-flight request: the canonical phase of the
/// exact request being sent followed by `...`, then the canonical role of the
/// provider serving it in brackets, then two grouped figures, separated by the
/// literal ` ~ ` -- the size of the context the request carries in plain bytes,
/// then an approximate token count (`context_bytes / BYTES_PER_TOKEN`,
/// floored). For example, `code... [mid] (12,345 B ~ 3,086 tokens)`.
///
/// `phase` is the same typed value the caller used to select the provider and
/// prepare the outgoing request, so the prefix and the request can never
/// disagree; a retry re-renders the prefix from that same phase. The formatter
/// reads only [`Phase::label`], so the prefix is one of the closed canonical
/// names (`plan...`, `spec...`, `tickets...`, `triage...`, `code...`,
/// `review...`, `summary...`) and no free-form text can reach it.
///
/// `role` is the canonical lowercase provider-role name -- `mid`, `giga`
/// or `smoll` -- of the provider the request is about to go to. The label only
/// renders it: resolving the role from the mode and the request phase is the
/// caller's job.
///
/// `input_price_per_million_usd` is the selected provider's own optional
/// per-million input rate, resolved by the caller from the same routing that
/// selected the provider. `None` means that provider is unpriced, so no
/// placeholder, currency symbol or zero estimate is rendered and the label is
/// byte for byte the prior no-price shape. `Some(0.0)` is a configured rate
/// and therefore renders `$0.000000 input`. When a rate is present the label
/// appends an input-only USD estimate in the form
/// ` ~ $<six fractional digits> input`; a Code request served by the Mid role
/// with a 12,345-byte context and a rate of 2.5 per million input tokens reads
/// `code... [mid] (12,345 B ~ 3,086 tokens ~ $0.007715 input)`.
///
/// The estimate is calculated from the displayed approximate token count --
/// `context_bytes / BYTES_PER_TOKEN`, floored to an integer -- multiplied by
/// the per-million rate and divided by one million. It is not derived directly
/// from bytes and not from provider-reported usage. The amount always has six
/// fractional digits, the `$` denotes USD, and `input` makes clear that output
/// tokens are excluded.
///
/// The byte figure is plain bytes: no unit scaling (KiB/MiB) and no rounding.
/// The token figure is a floor-division heuristic, not a tokenizer count and
/// not an exact, model-specific value. A rate is expected to be finite;
/// validating or rejecting an invalid configured rate belongs to the resolver
/// boundary. The assembled label is capped by [`truncate_row`] at `MAX_LABEL`
/// characters, so even an enormous context or an over-budget estimate is
/// truncated with the trailing ellipsis and cannot wrap, while complete figures
/// are not guaranteed past `MAX_LABEL`.
pub fn thinking_label(
    phase: Phase,
    role: &str,
    context_bytes: usize,
    input_price_per_million_usd: Option<f64>,
) -> String {
    let tokens = context_bytes / BYTES_PER_TOKEN;
    let composed = match input_price_per_million_usd {
        Some(rate) => {
            let input_cost_usd = tokens as f64 * rate / TOKENS_PER_MILLION;
            format!(
                "{}... [{}] ({} B ~ {} tokens ~ ${:.6} input)",
                phase.label(),
                role,
                group_digits(context_bytes),
                group_digits(tokens),
                input_cost_usd
            )
        }
        None => format!(
            "{}... [{}] ({} B ~ {} tokens)",
            phase.label(),
            role,
            group_digits(context_bytes),
            group_digits(tokens)
        ),
    };
    truncate_row(composed)
}

/// Render `value` in decimal with a `,` between groups of three digits counted
/// from the right: `12345 -> "12,345"`. No unit scaling and no rounding; the
/// same grouping is reused for both of [`thinking_label`]'s figures. A
/// hand-rolled insert that never places a separator before the leading group,
/// renders `0` as `0` with no leading zeros, and adds no dependency.
fn group_digits(value: usize) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    let len = digits.len();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Longest label we are willing to print. One row is only guaranteed by
/// staying well under any plausible terminal width; a wrapped label would
/// survive the `\r\x1b[K` erase as a partial row above the cursor.
const MAX_LABEL: usize = 60;

/// Longest permanent tool-call echo line (`$ ...`). Call lines are permanent
/// stdout entries rather than in-place erased status labels, so they have room
/// to show the step counter, tool prefix and collapsed multi-line body.
const MAX_CALL_LINE: usize = 100;

const ELLIPSIS: &str = "...";

/// Prefix on every non-empty line of agent (model) narration. It is the
/// transcript marker a redirected run keeps: a log file or a pipe gets the
/// same `#` column an interactive transcript always used to show. A terminal
/// does not, since it gets styled Markdown instead.
const AGENT_PREFIX: &str = "# ";

/// A one-row label for a tool call: `running: <target>`, whitespace and
/// control characters collapsed to single spaces (a model-emitted `ESC` inside
/// a command must not be able to drive the terminal), capped so it cannot wrap.
pub fn running_label(target: &str) -> String {
    let target = sanitize(target);
    if target.is_empty() {
        "running...".to_string()
    } else {
        truncate_row(format!("running: {}", target))
    }
}

/// Collapse all lines, control characters and runs of whitespace into single
/// spaces so multi-line scripts and commands remain visible on one row.
fn sanitize(target: &str) -> String {
    target
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Cap a label at `MAX_LABEL` characters (not bytes, so a multi-byte command
/// is never cut mid-character), ellipsis included in the budget.
fn truncate_row(label: String) -> String {
    truncate_at(label, MAX_LABEL)
}

/// Cap `label` at `max_chars` characters, ellipsis included in the budget.
fn truncate_at(label: String, max_chars: usize) -> String {
    if label.chars().count() <= max_chars {
        return label;
    }
    let keep = max_chars - ELLIPSIS.chars().count();
    let mut out: String = label.chars().take(keep).collect();
    out.push_str(ELLIPSIS);
    out
}

/// A permanent one-row echo of a tool call, sanitized and capped at
/// [`MAX_CALL_LINE`] characters. This is the transcript entry the user keeps;
/// the tool's output stays in the message history and never reaches the screen.
///
/// An executed command (`elapsed` is `Some`) renders its measurement between the
/// step counter and the call marker:
/// `[step/max] (seconds.millis s) $ <display>`, for example
/// `[3/100] (1.234s) $ cargo test`. The fraction is [`call_time`]'s -- whole
/// seconds and exactly three floored millisecond digits, glued to the unit -- so
/// the duration never becomes a second line or a second unit. A rejected call
/// (`elapsed` is `None`) renders `[step/max] $ <function name>` instead: it never
/// ran, so it must not imply a duration.
///
/// Together with [`agent_text`] this is the transcript convention: a call line
/// heads its display with the step counter and `$`, and `#` heads the model. The
/// markers are distinct at column 0, so however long a reply runs it can never be
/// confused with a call line.
///
/// The string this function returns carries no escape sequence on any kind of
/// stdout: colour is a decision about the *stream* the line is about to reach,
/// taken by the output sink, not part of the payload. The coordinator wraps this
/// payload with [`render_tool_call`] when that sink's stdout is a terminal.
pub fn call_line(
    step: usize,
    max_steps: usize,
    display: &str,
    elapsed: Option<Duration>,
) -> String {
    let display = sanitize(display);
    let counter = format!("[{step}/{max_steps}]");
    let mut line = match (elapsed, display.is_empty()) {
        (Some(elapsed), true) => format!("{counter} ({}) $", call_time(elapsed)),
        (Some(elapsed), false) => {
            format!("{counter} ({}) $ {display}", call_time(elapsed))
        }
        (None, true) => format!("{counter} $"),
        (None, false) => format!("{counter} $ {display}"),
    };
    if line.chars().count() > MAX_CALL_LINE {
        line = truncate_at(line, MAX_CALL_LINE);
    }
    line
}

/// One executed command's inline duration: whole seconds, a dot and exactly
/// three decimal milliseconds, with the `s` unit glued on (`1.234s`).
///
/// The whole seconds are the duration's own and the fraction is its millisecond
/// component floored -- finer time is dropped rather than rounded up -- and
/// zero-padded to three digits, so the width never changes: zero and any
/// sub-millisecond duration read `0.000s`, one millisecond reads `0.001s`, and
/// a duration past a minute or an hour keeps counting seconds (`61.500s`, never
/// `1:01.500`). No grouping is inserted, so 12,345 seconds reads `12345.678s`.
fn call_time(elapsed: Duration) -> String {
    format!("{}.{:03}s", elapsed.as_secs(), elapsed.subsec_millis())
}

/// Render one tool-call payload for an explicit terminal decision: the isolated
/// seam behind the call line's colour, so unit tests need neither a terminal nor
/// a pseudo-terminal.
///
/// `payload` is a whole call line, `call_line`'s capped and control-character-
/// free result, and it is styled as one unit -- the step counter, inline
/// duration, `$ ` marker and display, all of it. It is not re-sanitized, re-truncated or otherwise rewritten here: the
/// two concerns are separate, and a payload is never coloured into a different
/// line.
///
/// `terminal` picks one of two output contracts:
///
/// - `true`: the complete payload, wrapped in the palette's cyan
///   (`TOOL_CALL_FG`) and closed by an immediate full reset
///   (`ResetColor`, `\x1b[0m`), so the colour stops where the call line stops
///   and cannot bleed into the tool's own output or into the next entry. The
///   reset belongs to this rendering, so it precedes any newline a caller adds.
/// - `false`: the payload exactly as it was handed over, byte for byte, with no
///   escape sequence anywhere -- what a pipe or a redirected log sees.
///
/// The decision is the stream's `IsTerminal`, the same question narration asks.
/// The status line's separate `TERM=dumb` opt-out is not consulted: it silences
/// the one-row status line, and a call line was never part of that.
///
/// Neither branch adds a line terminator: the coordinator's own newline ends the
/// line, exactly as it does for narration and logs.
pub fn render_tool_call(payload: &str, terminal: bool) -> String {
    if !terminal {
        return payload.to_string();
    }
    render_cyan(payload)
}

/// Render one complete thinking report for an explicit terminal decision: the
/// same complete-payload cyan contract as a complete call line, but selected
/// only for the semantic report context `thinking_log` supplies.
///
/// On a terminal the payload is unchanged and the palette's foreground
/// (`#7dcfff`, `TOOL_CALL_FG`) wraps all of it, closed by an immediate full
/// reset, so the colour ends before the coordinator's newline. On a redirected
/// stream the payload is returned byte for byte with no escape sequence.
fn render_thinking_report(payload: &str, terminal: bool) -> String {
    if !terminal {
        return payload.to_string();
    }
    render_cyan(payload)
}

/// Wrap a complete payload in the palette's cyan and close it with the full
/// reset. The two escapes are written by crossterm rather than spelled out
/// here, so a coloured line is the terminal's own encoding of the palette.
fn render_cyan(payload: &str) -> String {
    let mut rendered = String::with_capacity(payload.len() + MAX_TOOL_CALL_ESCAPES);
    let _ = SetForegroundColor(TOOL_CALL_FG).write_ansi(&mut rendered);
    rendered.push_str(payload);
    let _ = ResetColor.write_ansi(&mut rendered);
    rendered
}

/// Room for the two escapes [`render_cyan`] writes -- crossterm's truecolor
/// foreground selection and its full reset -- so a coloured line is built
/// without a reallocation. It is a capacity hint, not a contract: the escapes
/// are crossterm's to write, and a longer one only costs the copy.
const MAX_TOOL_CALL_ESCAPES: usize = 32;

/// The three recognized leading severity tags, with the conventional ANSI
/// foreground each one selects: `[INFO]` cyan (36), `[WARN]` yellow (33) and
/// `[ERROR]` red (31).
///
/// The escape is the plain SGR the conventional code names, spelled out rather
/// than taken from crossterm's `Color`, because crossterm renders even its
/// basic colours as 256-colour `38;5;N` selections while a severity marker is
/// expected to arrive as `36`, `33` or `31`. A tag counts only when it is
/// exactly at the start of a payload, and only the tag's own bytes are coloured.
const SEVERITY_TAGS: [(&str, &str); 3] = [
    ("[INFO]", "\u{1b}[36m"),
    ("[WARN]", "\u{1b}[33m"),
    ("[ERROR]", "\u{1b}[31m"),
];

/// Room for the foreground selection and the full reset [`render_log`] writes
/// around a recognized severity tag, so a coloured log line is built without a
/// reallocation. A capacity hint, not a contract.
const MAX_LOG_ESCAPES: usize = 16;

/// Render one log payload for an explicit terminal decision: the isolated seam
/// behind the severity marker's colour, so unit tests need neither a terminal
/// nor a pseudo-terminal.
///
/// A log payload is coloured only when it begins with one of the three exact
/// severity tags -- `[INFO]`, `[WARN]` or `[ERROR]` -- and then only the tag
/// itself is painted, in the conventional foreground `36`, `33` or `31`, closed
/// by an immediate full reset (`\x1b[0m`). Everything after the tag, the message
/// remainder, is reproduced byte for byte, whitespace, embedded newlines and
/// all. The tag must be at the very start: a differently cased tag (`[info]`),
/// an unrecognized one (`[DEBUG]`), a tag later in the line, or a payload with
/// no tag at all is returned unchanged.
///
/// `terminal` picks one of two output contracts:
///
/// - `true`: a recognized leading tag is the conventional severity colour,
///   immediately closed by the full reset, with the remainder untouched; an
///   unrecognized payload is returned as it stands.
/// - `false`: the payload exactly as it was handed over, byte for byte, with no
///   escape sequence anywhere -- what a pipe or a redirected log sees.
///
/// The decision is the stream's own `IsTerminal`: stdout for a stdout log,
/// stderr for a stderr diagnostic. Neither branch adds a line terminator; the
/// coordinator's framing supplies those.
pub fn render_log(payload: &str, terminal: bool) -> String {
    if !terminal {
        return payload.to_string();
    }
    let Some((tag, color)) = SEVERITY_TAGS
        .into_iter()
        .find(|(tag, _)| payload.starts_with(tag))
    else {
        return payload.to_string();
    };
    let mut rendered = String::with_capacity(payload.len() + MAX_LOG_ESCAPES);
    rendered.push_str(color);
    rendered.push_str(tag);
    let _ = ResetColor.write_ansi(&mut rendered);
    rendered.push_str(&payload[tag.len()..]);
    rendered
}

/// One finished operation's elapsed line: `[INFO] {label} elapsed:
/// {whole_seconds}.{millis:03} seconds`, for example `[INFO] bash elapsed: 1.234
/// seconds`.
///
/// The whole seconds are the duration's own and the fraction is its millisecond
/// component floored -- finer time is dropped rather than rounded up -- and
/// zero-padded to three digits so the fraction is the same width on every line.
/// A duration below a millisecond therefore reads `0.000`, and one of 61.5
/// seconds reads `61.500`: no unit ever changes, on either side of a second or
/// of a minute.
///
/// Only the line is built here. Which stream it reaches, and whether its
/// `[INFO]` marker is coloured, is the output coordinator's decision, exactly as
/// it is for every other log line.
pub fn elapsed_line(label: &str, elapsed: Duration) -> String {
    format!(
        "[INFO] {} elapsed: {}.{:03} seconds",
        label,
        elapsed.as_secs(),
        elapsed.subsec_millis()
    )
}

/// One completed model attempt's compact metadata report:
/// `#thinking (<seconds>.<milliseconds>s) <provider> model=<model>
/// phase=<phase> context=<bytes> B`, for example
/// `#thinking (1.234s) giga model=example-model phase=code context=12,345 B`.
///
/// The duration is the attempt's own send-plus-decode interval, rendered by
/// the same floored whole-seconds-and-three-millisecond-digits rule as
/// [`call_time`]: zero and sub-millisecond durations read `0.000s`, and a
/// duration past a minute or an hour keeps counting ungrouped seconds
/// (`61.500s`, `3600.000s`). The context is the prepared request's
/// `context_bytes`, grouped by thousands with the same `,` rule as the
/// in-flight status label (`0 B`, `1 B`, `12,345 B`); it is never
/// estimated from tokens or recomputed from the outgoing body.
///
/// `provider` is the canonical lowercase role -- `mid`, `giga` or `smoll`;
/// `model` is the configured identifier for that route; `phase` is the
/// canonical lowercase phase label. The payload begins with `#thinking`, not
/// a recognized severity tag, so the coordinator writes it without severity
/// colouring. Only the line is built here: its stream, framing, flush and
/// ordering remain the output coordinator's decisions, exactly as they are for
/// every other log payload.
pub fn thinking_report(
    provider: &str,
    model: &str,
    phase: &str,
    context_bytes: usize,
    elapsed: Duration,
) -> String {
    format!(
        "#thinking ({}) {} model={} phase={} context={} B",
        call_time(elapsed),
        provider,
        model,
        phase,
        group_digits(context_bytes)
    )
}

/// A block of agent (model) narration, ready to print, for the stdout this
/// process actually has: Markdown styled by the module's one canonical Tokyo
/// Night skin (`tokyo_night_skin`) when stdout is a terminal, and the
/// `# `-prefixed plain text otherwise.
///
/// This is the production selector: it asks [`IsTerminal`] about *stdout*, and
/// it deliberately ignores the status line's `TERM=dumb` check -- a `dumb`
/// terminal still gets its narration as Markdown, exactly like any other
/// terminal, while the one-row status line stays off. The decision itself is
/// taken in [`render_agent_text`], which takes the answer as an argument so
/// tests can pin both branches without a terminal, a pseudo-terminal, or any
/// mutation of the real stdout.
pub fn agent_text(content: &str) -> String {
    render_agent_text(content, io::stdout().is_terminal())
}

/// Render one block of agent (model) narration for an explicit terminal
/// decision: the isolated seam behind [`agent_text`].
///
/// `terminal` picks one of two output contracts:
///
/// - `true`: the original, unprefixed narration rendered as Markdown by the
///   module's canonical Tokyo Night skin (`tokyo_night_skin`) -- no `# `
///   marker is added, so headings, emphasis and lists are interpreted and
///   styled rather than shown literally. Embedded blank lines survive as blank
///   lines.
/// - `false`: the established plain-text transcript form, `# ` before every
///   non-empty line, blank and whitespace-only lines left untouched. It
///   contains no escape sequences, so a redirected log or a pipe stays clean.
///
/// Neither branch adds a line terminator of its own: the caller's `println!`
/// supplies exactly one, terminal or not.
///
/// Together with [`call_line`] this is the transcript convention: a call line
/// carries its step counter and a `$ ` marker before the command display, and
/// `#` heads the model's narration. The markers are distinct, so however long a
/// reply runs it can never be confused with a call line.
pub fn render_agent_text(content: &str, terminal: bool) -> String {
    if terminal {
        markdown_agent_text(content)
    } else {
        plain_agent_text(content)
    }
}

/// The plain-text narration contract: `# ` before every line that carries
/// text, blank lines left untouched, and no trailing newline added.
///
/// Unlike the one-row call label, narration is neither sanitised nor truncated
/// -- it is reproduced verbatim, exactly as it was before the prefix existed,
/// trailing whitespace and trailing newlines included.
fn plain_agent_text(content: &str) -> String {
    content
        .split('\n')
        .map(|line| {
            if line.trim().is_empty() {
                line.to_string()
            } else {
                format!("{}{}", AGENT_PREFIX, line)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Ordinary narration text, prose, and everything the palette below does not
/// name: Tokyo Night's foreground `#c0caf5`, a soft off-white that stays
/// legible on the dark backgrounds this palette is built for.
const ORDINARY_FG: Color = Color::Rgb {
    r: 0xc0,
    g: 0xca,
    b: 0xf5,
};

/// Every heading level: Tokyo Night blue `#7aa2f7`. Level is never carried by
/// the colour -- see `tokyo_night_skin` -- so the whole heading scale shares
/// one, cohesive blue.
const HEADING_FG: Color = Color::Rgb {
    r: 0x7a,
    g: 0xa2,
    b: 0xf7,
};

/// Emphasis, strong and italic alike: Tokyo Night magenta `#bb9af7`. The two
/// forms stay apart by attribute -- bold for `**strong**`, italic for
/// `*emphasis*` -- not by colour.
const EMPHASIS_FG: Color = Color::Rgb {
    r: 0xbb,
    g: 0x9a,
    b: 0xf7,
};

/// Code, inline and fenced: Tokyo Night yellow `#e0af68`, the palette's
/// highest-contrast hue against a dark background, which is what a fragment
/// the model wants run deserves.
const CODE_FG: Color = Color::Rgb {
    r: 0xe0,
    g: 0xaf,
    b: 0x68,
};

/// The palette's cyan: Tokyo Night `#7dcfff`. On a terminal it is the
/// foreground of a whole tool-call line ([`render_tool_call`]), which is what
/// makes the call entries stand out from the model's ordinary narration without
/// changing one character of the line itself.
///
/// It is also the colour the palette names for links, and nothing in a rendered
/// narration consumes it -- a fact about termimad 0.35.5 rather than an omission
/// here: its Markdown model has no link element. The parser behind it
/// (minimad 0.16) parses no link syntax at all, so a `[label](url)` arrives as
/// ordinary text, and `MadSkin` exposes no link slot to paint even if it did.
/// The narration tests pin both this value and the rendering a link actually
/// gets.
const TOOL_CALL_FG: Color = Color::Rgb {
    r: 0x7d,
    g: 0xcf,
    b: 0xff,
};

/// The one canonical Markdown skin for terminal narration: the Tokyo Night
/// palette above applied to termimad's own style model.
///
/// A skin is built per call, from `MadSkin::default()`. Starting from the
/// default is what keeps everything the palette does not name -- paragraph and
/// list margins, bullet and quote characters, rules, tables, the heading-level
/// alignment -- rendering exactly as termimad renders it today; the styles
/// below are the only behaviour this module changes. Narration is rendered
/// once per model reply, so nothing here is worth caching.
///
/// The palette is foreground only. No style introduced here sets a background,
/// and the two backgrounds the default skin does carry -- the grey behind
/// inline code and behind a fenced code block -- are cleared, so the module
/// never paints over a background the user chose.
///
/// Attributes, as the palette requires:
///
/// - ordinary narration text and struck-out text take [`ORDINARY_FG`];
/// - `**strong**` keeps termimad's bold and `*emphasis*` its italic, both in
///   [`EMPHASIS_FG`];
/// - inline code and fenced code take [`CODE_FG`] with no background;
/// - every heading level is bold and blue, and the levels stay apart by
///   attribute: level one adds an underline, level two is plain bold, and
///   level three and deeper add italic. The attributes are set from scratch
///   rather than inherited from the default skin, so the headings cannot drift
///   with a termimad upgrade. The colour is shared on purpose -- a heading
///   scale that changes hue per level reads as decoration rather than
///   structure.
///
/// Links have no slot to occupy; [`TOOL_CALL_FG`] records why.
fn tokyo_night_skin() -> MadSkin {
    let mut skin = MadSkin::default();

    skin.paragraph.compound_style.set_fg(ORDINARY_FG);
    skin.strikeout.set_fg(ORDINARY_FG);

    skin.bold.set_fg(EMPHASIS_FG);
    skin.italic.set_fg(EMPHASIS_FG);

    skin.inline_code.set_fg(CODE_FG);
    skin.inline_code.object_style.background_color = None;
    skin.code_block.compound_style.set_fg(CODE_FG);
    skin.code_block.compound_style.object_style.background_color = None;

    for (level, header) in skin.headers.iter_mut().enumerate() {
        header.compound_style.set_fg(HEADING_FG);
        header.compound_style.remove_attr(Attribute::Underlined);
        header.compound_style.add_attr(Attribute::Bold);
        if level == 0 {
            header.compound_style.add_attr(Attribute::Underlined);
        } else if level >= 2 {
            header.compound_style.add_attr(Attribute::Italic);
        }
    }

    skin
}

/// The terminal narration contract: the original, unprefixed Markdown rendered
/// by termimad with the module's one canonical Tokyo Night skin
/// (`tokyo_night_skin`).
///
/// termimad still does the parsing and the layout; only the styles come from
/// this module. termimad terminates the block it renders with a newline. The
/// transcript contract here is that a renderer adds no line terminator of its
/// own, so the termimad-supplied one is dropped and the caller's `println!`
/// puts it back -- which is what keeps narration and call lines terminated
/// identically. An empty input renders to nothing at all.
fn markdown_agent_text(content: &str) -> String {
    // `MadSkin::text` is termimad's entry point for a skin of your own: it
    // parses the Markdown and formats it with that skin, where the
    // `termimad::text` convenience path would format it with the crate's
    // default one. Rendering it into a `String` (rather than printing it)
    // keeps the write -- and the decision behind it -- in the caller's hands.
    let rendered = tokyo_night_skin().text(content, None).to_string();
    match rendered.strip_suffix('\n') {
        Some(without_terminator) => without_terminator.to_string(),
        None => rendered,
    }
}

/// The status line itself. `Drop` erases, so the error returns and the `break`
/// inside the loop -- and any exit added later -- cannot leave it dangling.
pub struct Status {
    /// False when stdout is not a usable terminal: every method then no-ops.
    enabled: bool,
    /// True while the label is actually on screen.
    shown: bool,
}

impl Default for Status {
    fn default() -> Self {
        Self::new()
    }
}

impl Status {
    pub fn new() -> Status {
        // `TERM` unset is treated as fine (we already know it is a tty);
        // only an explicit `dumb` opts out, for terminals that would print
        // the escapes literally. Narration does not read this: its own switch
        // is stdout's `IsTerminal`, and the two are deliberately independent.
        let dumb = env::var("TERM")
            .map(|term| term.trim().eq_ignore_ascii_case("dumb"))
            .unwrap_or(false);

        Status {
            enabled: io::stdout().is_terminal() && !dumb,
            shown: false,
        }
    }

    /// Draw `label` as the last line of the transcript. Called immediately
    /// before every blocking operation, so it always starts from column 0.
    pub fn set(&mut self, label: &str) {
        if !self.enabled {
            return;
        }
        self.erase();
        print!("{}", label);
        self.flush();
        self.shown = true;
    }

    /// Erase the label. Idempotent, so it is safe to call before every write
    /// and on every exit path; it must run *before* any output, or the output
    /// would be appended to the label's row (and the erase would then eat the
    /// last line of that output).
    pub fn clear(&mut self) {
        if !self.enabled {
            return;
        }
        self.erase();
    }

    /// Time `operation`, report its elapsed duration as one
    /// `[INFO] <label> elapsed: ...` line, erasing the running label first, and
    /// return the operation's value unchanged.
    ///
    /// The clear comes before the report because the report is a real line: a
    /// line printed while the label was still up would be appended to the
    /// label's own row, and the erase that follows would take the timing line
    /// with it. What is measured is the operation alone
    /// ([`crate::timing::measure`]), so neither the label nor the report is part
    /// of the duration, and the report's own write errors are ignored exactly as
    /// every other informational call site ignores one.
    pub fn timed<T>(&mut self, label: &str, operation: impl FnOnce() -> T) -> T {
        crate::timing::measure(&mut ElapsedAfterClear { status: self }, label, operation)
    }

    fn erase(&mut self) {
        if self.shown {
            print!("\r\x1b[K");
            self.flush();
            self.shown = false;
        }
    }

    /// stdout is line-buffered and the label has no newline, so without this
    /// the label would sit in the buffer until something else flushed.
    fn flush(&self) {
        let _ = io::stdout().flush();
    }
}

impl Drop for Status {
    fn drop(&mut self) {
        self.clear();
    }
}

/// The report side of [`Status::timed`]: erase the running label, then write the
/// elapsed line through the process-wide coordinator.
///
/// It is the production [`crate::timing::Reporter`] for a timed operation that has
/// a label above it, which is exactly the agent loop's tool call. The label each
/// report carries comes from the measurement, so the only state here is the
/// status line that has to go first; on a stream that never shows a label,
/// [`Status::clear`] is a no-op and only the line is written.
struct ElapsedAfterClear<'a> {
    status: &'a mut Status,
}

impl crate::timing::Reporter for ElapsedAfterClear<'_> {
    fn report(&mut self, label: &str, elapsed: Duration) {
        self.status.clear();
        let _ = shared().elapsed_log(label, elapsed);
    }
}

/// Time operation, report its elapsed duration as one
/// [INFO] <label> elapsed: ... line, and return the operation's value unchanged.
///
/// This is the phase-level sibling of Status::timed: the pipeline's own phases
/// have no running label above them to erase, so the report goes straight to the
/// process-wide coordinator. The measuring itself is crate::timing::measure's,
/// so what is timed is the operation alone -- neither the work a caller does
/// before it nor the report that follows it is part of the duration -- and the
/// operation's value comes back exactly as it was produced, on every outcome.
///
/// The shared lock is taken only for the report, never while the operation runs,
/// so a timed phase stays free to print through the same coordinator. The
/// report's own write error is ignored exactly as every other informational call
/// site ignores one.
pub fn timed<T>(label: &str, operation: impl FnOnce() -> T) -> T {
    let mut reporter = PhaseElapsed;
    crate::timing::measure(&mut reporter, label, operation)
}

/// The production crate::timing::Reporter for a timed phase invocation: one
/// elapsed line, written through the process-wide coordinator.
struct PhaseElapsed;

impl crate::timing::Reporter for PhaseElapsed {
    fn report(&mut self, label: &str, elapsed: Duration) {
        let _ = shared().elapsed_log(label, elapsed);
    }
}

// ---------------------------------------------------------------------------
// Output coordination
// ---------------------------------------------------------------------------

use std::sync::{Mutex, MutexGuard};

/// The three kinds of user-visible output the coordinator classifies.
///
/// Everything the process shows the user is one of these: a tool call (the
/// permanent call line built by [`call_line`]), model narration (the block rendered by
/// [`agent_text`]), or a log. Neither severity nor stream creates a fourth
/// kind: `[INFO]`, `[WARN]` and `[ERROR]`, on stdout or on stderr, are all
/// [`OutputKind::Log`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputKind {
    /// One permanent tool-call transcript line.
    ToolCall,
    /// One block of the model's own narration.
    Narration,
    /// A log message: a startup notice, a stage banner, a summary, a warning
    /// or an error. One kind whatever the severity and whichever stream
    /// carries it.
    Log,
}

/// The byte sink an [`OutputCoordinator`] writes through.
///
/// Six operations are enough for the coordinator's policy: ask each stream
/// whether it is a terminal, then append to and flush it. Production uses
/// [`StdioSink`]; a unit test injects a recording sink, so the coordinator's
/// bytes, flush order and rendering can be asserted without a terminal and
/// without touching the process's real streams. Every operation is fallible,
/// because the coordinator only remembers a kind once the message carrying it
/// has actually been written.
pub trait OutputSink {
    /// Whether this sink's stdout is a terminal, asked independently of
    /// stderr.
    ///
    /// It decides three things: whether a tool-call payload is coloured
    /// ([`render_tool_call`]), whether a complete thinking report is coloured
    /// (`render_thinking_report`), and whether a recognized leading severity tag
    /// in a stdout log is coloured ([`render_log`]) before any of them is
    /// written. Narration is not coloured from here -- its own switch is
    /// stdout's `IsTerminal` where the payload is built ([`agent_text`]).
    ///
    /// The default is `false`, the answer that keeps the bytes plain: a sink
    /// that cannot tell the two apart, or that does not care, writes exactly
    /// what it was handed.
    fn stdout_is_terminal(&self) -> bool {
        false
    }

    /// Whether this sink's stderr is a terminal, asked independently of stdout.
    ///
    /// The two streams are redirected separately -- `2>` a log file while the
    /// transcript stays on screen, or the other way round -- so the answer is
    /// per stream and never inherited from the other one. It decides exactly
    /// one thing: whether a recognized leading severity tag in a stderr
    /// diagnostic is coloured ([`render_log`]) before the diagnostic is
    /// written.
    fn stderr_is_terminal(&self) -> bool {
        false
    }

    /// Append `bytes` to stdout.
    fn write_stdout(&mut self, bytes: &str) -> io::Result<()>;

    /// Flush stdout.
    fn flush_stdout(&mut self) -> io::Result<()>;

    /// Append `bytes` to stderr.
    fn write_stderr(&mut self, bytes: &str) -> io::Result<()>;

    /// Flush stderr.
    fn flush_stderr(&mut self) -> io::Result<()>;
}

/// The production [`OutputSink`]: this process's real stdout and stderr.
///
/// It holds no state: each operation takes and releases the per-stream lock,
/// exactly as a direct `println!` would, so output is interleaved with any
/// other writer the same way it always was.
///
/// The two terminal-capability queries are answered from the real streams,
/// each on its own: [`io::stdout`] and [`io::stderr`] are redirected
/// independently, so neither answer may stand in for the other.
#[derive(Debug, Default, Clone, Copy)]
pub struct StdioSink;

impl OutputSink for StdioSink {
    fn stdout_is_terminal(&self) -> bool {
        io::stdout().is_terminal()
    }

    fn stderr_is_terminal(&self) -> bool {
        io::stderr().is_terminal()
    }

    fn write_stdout(&mut self, bytes: &str) -> io::Result<()> {
        io::stdout().lock().write_all(bytes.as_bytes())
    }

    fn flush_stdout(&mut self) -> io::Result<()> {
        io::stdout().lock().flush()
    }

    fn write_stderr(&mut self, bytes: &str) -> io::Result<()> {
        io::stderr().lock().write_all(bytes.as_bytes())
    }

    fn flush_stderr(&mut self) -> io::Result<()> {
        io::stderr().lock().flush()
    }
}

/// The canonical phases whose reports take part in the review/summary spacing
/// sequence. The names are exact and lowercase; anything else is an ordinary
/// report payload under the generic policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReportPhase {
    Review,
    Summary,
}

impl ReportPhase {
    /// The canonical phase a report labels itself with, or `None` when the
    /// label is unknown or differently cased.
    fn parse(phase: &str) -> Option<Self> {
        match phase {
            "review" => Some(Self::Review),
            "summary" => Some(Self::Summary),
            _ => None,
        }
    }
}

/// The semantic context of one stdout message. Only `thinking_log` supplies a
/// report phase; every other route writes a plain payload whose bytes the
/// coordinator never interprets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MessageContext {
    Plain,
    Report(ReportPhase),
    NoncanonicalReport,
}

/// What the immediately preceding successful stdout write was, for the
/// phase-aware separators. `Other` is the generic kind policy; the other
/// markers are the successful transitions of the review-to-summary sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SpacingMarker {
    Other,
    ReviewReport,
    ReviewInfo,
    SummaryReport,
    SummaryToolCall,
}

/// The centralized output coordinator: one remembered kind, and the spacing,
/// framing and flush policy that goes with it.
///
/// The stdout rule is the old transcript rule raised from one agent invocation
/// to the whole process. The coordinator writes the supplied payload, ended by
/// exactly one newline as a `println!` would have ended it, and prefixed by
/// exactly one empty line only when the remembered kind differs -- apart from
/// the documented review/summary transitions and the adjacency of a semantic
/// thinking report and the tool call directly after it. The first message of a
/// run and a message of the same kind get no separator. Nothing
/// is normalized: embedded newlines and caller-supplied formatting -- a banner
/// that carries its own blank lines, say -- survive verbatim, on top of
/// whatever separator the policy adds.
///
/// A stderr diagnostic is the same [`OutputKind::Log`] as a stdout log and
/// follows the same global event order: no separator after another log on
/// either stream, exactly one blank-line separator when the remembered kind is
/// not a log, and one terminating newline. The separator is written on the
/// stream that carries the event being written, so a stream change never adds
/// a second blank line. The diagnostic's bytes are untouched, internal
/// newlines included.
///
/// The bytes that depend on their destination are the tool call, the
/// thinking report and the log. A call line is written through
/// [`render_tool_call`], so it is the whole payload in the palette's cyan when
/// the sink's stdout is a terminal and the plain payload when it is not. A
/// report submitted through [`OutputCoordinator::thinking_log`] is written
/// through the same complete-payload cyan contract, selected by its semantic
/// report context rather than by inspecting the payload text. A generic log is
/// written through [`render_log`], so a recognized leading severity tag is the
/// conventional severity colour when the stream carrying it is a terminal and
/// every byte is plain when it is not; a log with no recognized tag is
/// untouched either way. The escape codes live *inside* the payload, before
/// the newline, so the separator and the terminator stay plain and narration is
/// written exactly as its caller handed it over.
///
/// The remembered kind is updated only after the message was written and
/// flushed, so a write that fails leaves the previous kind in place. A message
/// a caller skips never reaches the coordinator at all, so it writes nothing
/// and changes nothing.
pub struct OutputCoordinator<S = StdioSink> {
    sink: S,
    last: Option<OutputKind>,
    /// The phase-aware transition established by the last successful stdout
    /// write. It is committed only with `last`, after the write and flush
    /// succeed, so a failed message leaves the previous transition in place.
    spacing: SpacingMarker,
    /// Whether the last successful stdout write was a semantic thinking report
    /// submitted through [`OutputCoordinator::thinking_log`]. A tool call
    /// immediately after such a report is the report's own continuation and is
    /// written with no separator. Like `spacing`, this is committed only after
    /// both the stdout write and flush succeed, so a failed message leaves the
    /// previous predecessor in place. An ordinary [`OutputCoordinator::stdout_log`]
    /// is never semantic, however much its payload resembles a report.
    report_predecessor: bool,
}

impl<S: OutputSink> OutputCoordinator<S> {
    /// A coordinator over `sink` with no remembered kind: its first message is
    /// spaced as the first message of a run.
    pub const fn new(sink: S) -> Self {
        Self {
            sink,
            last: None,
            spacing: SpacingMarker::Other,
            report_predecessor: false,
        }
    }

    /// The kind of the last message that was successfully written, if any.
    pub fn last(&self) -> Option<OutputKind> {
        self.last
    }

    /// The sink, by shared reference: the seam a test uses to read back the
    /// bytes and operations a recording sink observed.
    pub fn sink(&self) -> &S {
        &self.sink
    }

    /// The sink, mutably: the seam a test uses to flip a controlled sink's
    /// failure switch between writes.
    pub fn sink_mut(&mut self) -> &mut S {
        &mut self.sink
    }

    /// Write one tool-call transcript line ([`call_line`]) to stdout.
    ///
    /// The payload is the plain line [`call_line`] built; on a terminal the
    /// coordinator writes it through [`render_tool_call`], painting the whole
    /// `$ <target>` line and resetting before its own newline. Which of the two
    /// renderings is used is not the caller's decision: it follows the sink's
    /// stdout.
    pub fn tool_call(&mut self, payload: &str) -> io::Result<()> {
        self.stdout(OutputKind::ToolCall, payload)
    }

    /// Write one block of already-rendered narration ([`agent_text`]) to
    /// stdout.
    pub fn narration(&mut self, payload: &str) -> io::Result<()> {
        self.stdout(OutputKind::Narration, payload)
    }

    /// Write one log line to stdout. `[INFO]`-style output keeps its stream
    /// and counts as [`OutputKind::Log`], sharing its global adjacency and
    /// log/non-log transition policy with a stderr diagnostic. The payload is
    /// rendered through [`render_log`] against the sink's stdout, so a
    /// recognized leading severity tag is coloured on a terminal and untouched
    /// on a redirected stream.
    pub fn stdout_log(&mut self, payload: &str) -> io::Result<()> {
        self.stdout(OutputKind::Log, payload)
    }

    /// Write one operation's elapsed duration to stdout as an informational log
    /// line ([`elapsed_line`]).
    ///
    /// The line is an ordinary [`OutputKind::Log`] payload: it keeps the spacing
    /// rule, the single terminator and the stream-dependent colouring of its
    /// `[INFO]` marker, so a redirected transcript carries exactly
    /// `[INFO] <label> elapsed: <seconds>.<millis> seconds` and nothing else.
    pub fn elapsed_log(&mut self, label: &str, elapsed: Duration) -> io::Result<()> {
        self.stdout_log(&elapsed_line(label, elapsed))
    }

    /// Write one completed model attempt's compact metadata report
    /// ([`thinking_report`]) to stdout as an ordinary log payload.
    ///
    /// The report keeps the coordinator's normal `OutputKind::Log` framing --
    /// the single terminating newline, the blank-line separator when the
    /// remembered kind differs, and the stdout flush. Its `#thinking` payload
    /// has no recognized severity tag, so it is never severity-coloured; on a
    /// terminal stdout the complete payload instead takes the same palette cyan
    /// as a complete call line, with the full reset before the newline, while a
    /// redirected stdout receives the exact plain payload. The terminal answer
    /// is stdout's own and is never taken from stderr or from the status line's
    /// `TERM=dumb` opt-out. Nothing about the attempt itself is measured here:
    /// the duration is supplied by the caller's attempt timing.
    pub fn thinking_log(
        &mut self,
        provider: &str,
        model: &str,
        phase: &str,
        context_bytes: usize,
        elapsed: Duration,
    ) -> io::Result<()> {
        let report = thinking_report(provider, model, phase, context_bytes, elapsed);
        let context = match ReportPhase::parse(phase) {
            Some(phase) => MessageContext::Report(phase),
            None => MessageContext::NoncanonicalReport,
        };
        self.write(OutputKind::Log, &report, context)
    }

    /// Write one stdout message of kind `kind`: the payload and one newline,
    /// preceded by exactly one empty line when the remembered kind differs.
    ///
    /// The coordinator boundary is where a payload is turned into bytes for a
    /// stream, and it is the only place that colours anything: a
    /// [`OutputKind::ToolCall`] payload goes through [`render_tool_call`], a
    /// [`OutputKind::Log`] payload carrying a report context goes through the
    /// complete-payload cyan renderer, and every other [`OutputKind::Log`]
    /// payload goes through [`render_log`], while narration is written byte for
    /// byte as handed over. The report context is semantic, so a generic log
    /// that merely looks like a report is never coloured as one. The separator
    /// in front of the payload and the newline after it are never styled, so
    /// any colour begins exactly at the payload and ends before the line does.
    pub fn stdout(&mut self, kind: OutputKind, payload: &str) -> io::Result<()> {
        self.write(kind, payload, MessageContext::Plain)
    }

    /// Write one stdout message with its private semantic context: the
    /// separator chosen by the phase-aware policy, the same rendering and
    /// flush policy as every other message, and a transition committed only
    /// after both the sink write and the flush succeed.
    fn write(
        &mut self,
        kind: OutputKind,
        payload: &str,
        context: MessageContext,
    ) -> io::Result<()> {
        // Only the exact informational tag takes part in the review sequence.
        // An untagged or differently tagged log is an ordinary message under
        // the generic kind policy, even when it follows a review report.
        let information = payload.starts_with("[INFO]");
        let (separator, next_spacing) = self.spacing_for(kind, context, information);
        // A previous log may have gone to stderr, so stdout is only written
        // once that stream is flushed: otherwise the diagnostic and the
        // message that follows it can swap places in a combined or redirected
        // transcript.
        if self.last == Some(OutputKind::Log) {
            self.sink.flush_stderr()?;
        }
        // The terminal answer is taken from the sink, so a test can pin the
        // coloured and the plain rendering without owning a terminal. Only the
        // payload is rendered: the framing around it stays plain text. A report
        // is identified by the semantic context supplied by its reporting path,
        // never by inspecting the payload prefix, so a generic lookalike stays
        // an ordinary log.
        let terminal = self.sink.stdout_is_terminal();
        let payload = match (kind, context) {
            (OutputKind::ToolCall, _) => render_tool_call(payload, terminal),
            (OutputKind::Log, MessageContext::Report(_))
            | (OutputKind::Log, MessageContext::NoncanonicalReport) => {
                render_thinking_report(payload, terminal)
            }
            (OutputKind::Log, MessageContext::Plain) => render_log(payload, terminal),
            (OutputKind::Narration, _) => payload.to_string(),
        };
        self.sink.write_stdout(&format!("{separator}{payload}\n"))?;
        self.sink.flush_stdout()?;
        self.last = Some(kind);
        self.spacing = next_spacing;
        self.report_predecessor = matches!(
            context,
            MessageContext::Report(_) | MessageContext::NoncanonicalReport
        );
        Ok(())
    }

    /// The generic kind separator: nothing for the first message and for a
    /// repeat of the remembered kind, one blank line for a change.
    fn generic_separator(&self, kind: OutputKind) -> &'static str {
        match self.last {
            Some(previous) if previous == kind => "",
            Some(_) => "\n",
            None => "",
        }
    }

    /// The separator and next committed transition for one stdout message.
    ///
    /// Only the successful review-to-summary sequence changes the generic
    /// rule: the first review information line after a review report, the
    /// first summary report after that information block, and the alternating
    /// summary report/tool-call pairs -- a summary report immediately followed
    /// by a call line, then that call line immediately followed by the next
    /// summary report.
    ///
    /// The one exception is the initial pair entry: the very first message of
    /// a run may be a summary report, and the call line immediately after it
    /// stays adjacent. Once any message has been written, a summary report in
    /// generic context cannot re-arm the pair transition: an interruption
    /// cannot be undone by another summary report. That report's own following
    /// call is still adjacent, because every semantic report keeps a directly
    /// following call adjacent, but the pair transition itself is not
    /// restored. Everything else, including a repeated report, a repeated
    /// call, or a log carrying no exact leading `[INFO]` tag, also falls back
    /// to [`Self::generic_separator`] and leaves the summary sequence.
    fn spacing_for(
        &self,
        kind: OutputKind,
        context: MessageContext,
        information: bool,
    ) -> (&'static str, SpacingMarker) {
        use MessageContext::{Plain, Report};
        use ReportPhase::{Review as ReviewPhase, Summary as SummaryPhase};
        use SpacingMarker::{Other, ReviewInfo, ReviewReport, SummaryReport, SummaryToolCall};

        match (self.spacing, kind, context) {
            // The first review information line after a review report gets one
            // blank line; the rest of the block stays adjacent. Only a log
            // whose payload begins with the exact `[INFO]` tag is review
            // information: an untagged or differently tagged log falls through
            // to the generic rule and ends the review sequence.
            (ReviewReport, OutputKind::Log, Plain) if information => {
                return ("\n", ReviewInfo);
            }
            (ReviewInfo, OutputKind::Log, Plain) if information => {
                return ("", ReviewInfo);
            }
            // The first summary report after the review information block gets
            // one blank line.
            (ReviewInfo, OutputKind::Log, Report(SummaryPhase)) => return ("\n", SummaryReport),
            // The summary exception is exactly the alternating pair: a
            // summary report immediately followed by its call line, and that
            // call line immediately followed by the next summary report. A
            // repeated report or a repeated call is not a pair, so it falls
            // through to the generic rule and the summary context ends there.
            (SummaryReport, OutputKind::ToolCall, Plain) => {
                return ("", SummaryToolCall);
            }
            (SummaryToolCall, OutputKind::Log, Report(SummaryPhase)) => {
                return ("", SummaryReport);
            }
            // A repeated summary report is not a pair either: it keeps the
            // generic separator for its unchanged kind, and the summary
            // context ends here rather than carrying over to the next call.
            (SummaryReport, OutputKind::Log, Report(SummaryPhase)) => {
                return (self.generic_separator(kind), Other);
            }
            _ => {}
        }

        // A semantic report immediately before a tool call is the report's own
        // continuation, so the call starts on the next line with no blank
        // separator. Report identity is the reporting path's context, never the
        // payload text, so an ordinary log lookalike keeps the generic
        // separator. The transition computed by the generic/phase-aware policy
        // is left exactly as it would have been: only the separator at this
        // boundary changes, so an interrupted summary pair is not re-armed and
        // the review sequence still ends where a call always ended it.
        let follows_report = kind == OutputKind::ToolCall && self.report_predecessor;

        let next = match (kind, context) {
            (OutputKind::Log, Report(ReviewPhase)) => ReviewReport,
            // Only the initial summary report of a run may enter a pair
            // without passing through the review information block. Every
            // other generic-context summary report -- including one after an
            // interruption -- cannot re-arm the pair exception. Its own
            // following call is still adjacent because it is a semantic
            // report, not because the pair was restored.
            (OutputKind::Log, Report(SummaryPhase)) if self.last.is_none() => SummaryReport,
            _ => Other,
        };
        // A semantic thinking report that directly follows a successfully
        // committed call line belongs to that call's block, so it starts on
        // the next line with no blank separator whatever its phase. Report
        // identity is the reporting path's context, never the payload text,
        // so an ordinary log lookalike keeps the generic separator. The
        // phase-aware transition above is preserved: only the separator
        // before the report changes.
        let follows_call = kind == OutputKind::Log
            && matches!(
                context,
                MessageContext::Report(_) | MessageContext::NoncanonicalReport
            )
            && self.last == Some(OutputKind::ToolCall);
        if follows_call || follows_report {
            return ("", next);
        }
        (self.generic_separator(kind), next)
    }

    /// Write one stderr diagnostic under the coordinator's global log policy:
    /// no separator after another log on either stream, exactly one blank-line
    /// separator when the remembered kind is not a log, and one terminating
    /// newline. The message's own bytes -- internal newlines included -- are
    /// untouched. The message is rendered through [`render_log`] against the
    /// sink's stderr, so a recognized leading severity tag is coloured on a
    /// terminal while the separator and terminator stay plain.
    pub fn stderr_log(&mut self, message: &str) -> io::Result<()> {
        // Buffered stdout has to land before the diagnostic, or the two
        // streams can reorder once the process is redirected or combined.
        self.sink.flush_stdout()?;
        let separator = self.generic_separator(OutputKind::Log);
        let message = render_log(message, self.sink.stderr_is_terminal());
        self.sink.write_stderr(&format!("{separator}{message}\n"))?;
        self.sink.flush_stderr()?;
        self.last = Some(OutputKind::Log);
        // A diagnostic is output between a report and any later call, so it
        // ends the report-to-call adjacency exactly as the old kind change did.
        self.report_predecessor = false;
        // A stderr diagnostic is not part of a summary pair, so it exits that
        // sequence. The review transition may still be pending: the diagnostic
        // does not write to stdout and does not consume the review information
        // block.
        if matches!(
            self.spacing,
            SpacingMarker::SummaryReport | SpacingMarker::SummaryToolCall
        ) {
            self.spacing = SpacingMarker::Other;
        }
        Ok(())
    }
}

/// The elapsed-line route as the coordinator's own operation: a
/// [`crate::timing::Reporter`] over an explicit coordinator, so a unit test can
/// time an operation against a recording sink and assert the exact bytes the
/// elapsed line becomes.
impl<S: OutputSink> crate::timing::Reporter for OutputCoordinator<S> {
    fn report(&mut self, label: &str, elapsed: Duration) {
        let _ = self.elapsed_log(label, elapsed);
    }
}

/// The process-wide production coordinator every call site shares.
///
/// Its remembered kind is the kind of the last user-visible message printed by
/// any part of the process -- startup, a pipeline stage, a queue ticket, one
/// agent loop, one REPL turn -- so spacing spans the whole run instead of
/// restarting wherever a component happens to begin. The lock is held for one
/// write; the crate is synchronous, so it never spans a blocking call.
static COORDINATOR: Mutex<OutputCoordinator<StdioSink>> =
    Mutex::new(OutputCoordinator::new(StdioSink));

/// Borrow the process-wide coordinator for one output operation.
///
/// Production call sites write through this and keep the established stream:
/// [`OutputCoordinator::stdout`] (or one of its `tool_call`, `narration` and
/// `stdout_log` forms) for stdout output, [`OutputCoordinator::stderr_log`]
/// for warnings and errors.
///
/// Unit tests do not use it: they build their own [`OutputCoordinator`] over a
/// recording sink, so they neither capture nor mutate the real streams and can
/// run in parallel. A poisoned lock is recovered from -- printing a message
/// must not fail because an unrelated thread panicked while printing.
pub fn shared() -> MutexGuard<'static, OutputCoordinator<StdioSink>> {
    COORDINATOR
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The block of model narration that should be printed, or `None` when the
/// reply carries nothing to narrate: a missing `content` or a whitespace-only
/// one.
///
/// A skipped narration is no message, so it must never be handed to the
/// coordinator: it writes no bytes and leaves the remembered kind alone, and
/// the next message is still spaced against the last one that reached the
/// screen.
pub fn printable_narration(content: Option<&str>) -> Option<&str> {
    content.filter(|text| !text.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three canonical provider-role names, rendered the way a caller
    /// passes them: lowercase, in brackets, never the configured mode.
    const ROLES: [&str; 3] = ["mid", "giga", "smoll"];

    /// The closed set of request phases, so exhaustive coverage is one shared
    /// list: a new variant has to be added here as well as to the formatter's
    /// own match, and every phase-driven label check picks it up.
    const PHASES: [Phase; 7] = [
        Phase::Plan,
        Phase::Spec,
        Phase::Tickets,
        Phase::Triage,
        Phase::Code,
        Phase::Review,
        Phase::Summary,
    ];

    /// The palette's foreground escapes exactly as they reach a terminal: a
    /// truecolor foreground selection, and the reset crossterm writes at the
    /// end of a run. The RGB triples are the palette in decimal -- `192;202;245`
    /// is `#c0caf5`, `122;162;247` is `#7aa2f7`, `187;154;247` is `#bb9af7`,
    /// `224;175;104` is `#e0af68`, and `125;207;255` is `#7dcfff` -- so a
    /// palette change has to be a deliberate edit here as well as in
    /// `tokyo_night_skin`.
    const ORDINARY: &str = "\u{1b}[38;2;192;202;245m";
    const HEADING: &str = "\u{1b}[38;2;122;162;247m";
    const EMPHASIS: &str = "\u{1b}[38;2;187;154;247m";
    const CODE: &str = "\u{1b}[38;2;224;175;104m";

    /// The palette's cyan, `#7dcfff`, as it reaches a terminal. It is the
    /// foreground of a tool-call line and -- the narration tests pin this --
    /// of nothing in a rendered narration.
    const CYAN: &str = "\u{1b}[38;2;125;207;255m";

    /// The conventional severity foregrounds as they reach a terminal: cyan
    /// (36) for `[INFO]`, yellow (33) for `[WARN]` and red (31) for `[ERROR]`.
    /// They are the plain SGR codes, not the palette's truecolor values, and
    /// each marker is closed by `STYLE_END`.
    const INFO_FG: &str = "\u{1b}[36m";
    const WARN_FG: &str = "\u{1b}[33m";
    const ERROR_FG: &str = "\u{1b}[31m";

    /// The end of a run that only set a foreground, and the end of a run that
    /// set attributes: two different resets, both written by crossterm. The
    /// second is the full reset (`0m`) that a coloured tool-call line closes
    /// with, so the colour ends at the end of the payload.
    const ORDINARY_END: &str = "\u{1b}[39m";
    const STYLE_END: &str = "\u{1b}[0m";

    /// The attributes the palette asks for.
    const BOLD: &str = "\u{1b}[1m";
    const ITALIC: &str = "\u{1b}[3m";
    const UNDERLINE: &str = "\u{1b}[4m";
    const CROSSED_OUT: &str = "\u{1b}[9m";

    /// How a background is set and cleared. No terminal narration may contain
    /// either: the palette is foreground-only.
    const BACKGROUND_SET: &str = "\u{1b}[48;";
    const BACKGROUND_RESET: &str = "\u{1b}[49m";

    /// What the module's canonical skin renders `content` as, with termimad's
    /// block terminator removed: the structural reference for the tests that
    /// care about a block's line layout rather than about which escape carries
    /// it. Styling is asserted through the literals above, so this helper
    /// cannot hide a palette change.
    fn styled_markdown(content: &str) -> String {
        tokyo_night_skin()
            .text(content, None)
            .to_string()
            .strip_suffix('\n')
            .unwrap_or_default()
            .to_string()
    }

    /// What termimad's *default* skin renders `content` as, terminator
    /// removed. It is used only to show that the terminal branch is not the
    /// default rendering: no test asserts the terminal output against it, so a
    /// change in termimad's own palette cannot make these tests fail.
    fn default_markdown(content: &str) -> String {
        termimad::text(content)
            .to_string()
            .strip_suffix('\n')
            .unwrap_or_default()
            .to_string()
    }

    /// Ordinary single-line narration gains the marker once and no trailing
    /// newline of its own.
    #[test]
    fn plain_narration_prefixes_a_single_line_without_a_trailing_newline() {
        assert_eq!(render_agent_text("hello", false), "# hello");
    }

    /// Every non-empty line is prefixed, and the lines are joined back with a
    /// single `\n`.
    #[test]
    fn plain_narration_prefixes_every_non_empty_line() {
        assert_eq!(render_agent_text("a\n\nb", false), "# a\n\n# b");
        assert_eq!(
            render_agent_text("one\ntwo\nthree", false),
            "# one\n# two\n# three"
        );
    }

    /// Blank and whitespace-only lines are neither prefixed nor rewritten:
    /// spaces and tabs survive exactly as written, and an empty block stays
    /// empty.
    #[test]
    fn plain_narration_leaves_blank_and_whitespace_only_lines_alone() {
        assert_eq!(render_agent_text("   \n\t\n", false), "   \n\t\n");
        assert_eq!(
            render_agent_text("a\n   \n\t\nb", false),
            "# a\n   \n\t\n# b"
        );
        assert_eq!(render_agent_text("", false), "");
    }

    /// `split('\n')` (not `lines()`) keeps a leading empty line as a blank one,
    /// however many of them there are.
    #[test]
    fn plain_narration_preserves_a_leading_empty_line() {
        assert_eq!(render_agent_text("\nx", false), "\n# x");
        assert_eq!(render_agent_text("\n\nx", false), "\n\n# x");
    }

    /// Trailing newline content is not trimmed: the input's own line structure
    /// survives unchanged, `\n` and all.
    #[test]
    fn plain_narration_keeps_trailing_newline_content() {
        assert_eq!(render_agent_text("a\n", false), "# a\n");
        assert_eq!(render_agent_text("a\n\n", false), "# a\n\n");
        assert_eq!(render_agent_text("a\nb\n", false), "# a\n# b\n");
    }

    /// The plain branch never emits an escape sequence, whatever the narration
    /// looks like -- Markdown syntax included, since a redirected transcript
    /// must stay readable as the text the model wrote.
    #[test]
    fn plain_narration_has_no_ansi_sequences() {
        let cases = [
            ("hello", "# hello"),
            ("a\n\nb", "# a\n\n# b"),
            ("   \n\t\n", "   \n\t\n"),
            ("\nx", "\n# x"),
            ("a\n", "# a\n"),
            ("**bold** and *italic*", "# **bold** and *italic*"),
            (
                "# Heading\n\n- one\n- two",
                "# # Heading\n\n# - one\n# - two",
            ),
        ];
        for (content, expected) in cases {
            let rendered = render_agent_text(content, false);
            assert_eq!(rendered, expected, "plain narration changed: {content:?}");
            assert!(
                !rendered.contains('\u{1b}'),
                "plain narration must not carry ANSI: {rendered:?}"
            );
        }
    }

    /// Ordinary terminal narration is the palette's off-white `#c0caf5`,
    /// applied per line and closed again at the end of each one, with no `# `
    /// marker anywhere.
    #[test]
    fn terminal_narration_uses_the_tokyo_night_ordinary_foreground() {
        assert_eq!(
            render_agent_text("hello", true),
            format!("{ORDINARY}hello{ORDINARY_END}")
        );
        assert_eq!(
            render_agent_text("one\ntwo\nthree", true),
            format!(
                "{ORDINARY}one{ORDINARY_END}\n\
                 {ORDINARY}two{ORDINARY_END}\n\
                 {ORDINARY}three{ORDINARY_END}"
            )
        );

        // Struck-out text is ordinary text with an attribute, so it keeps the
        // ordinary foreground rather than falling back to the terminal's.
        assert_eq!(
            render_agent_text("~~gone~~", true),
            format!("{ORDINARY}{CROSSED_OUT}gone{STYLE_END}")
        );

        let rendered = render_agent_text("hello", true);
        assert!(
            !rendered.starts_with(AGENT_PREFIX),
            "no marker on a terminal: {rendered:?}"
        );
        assert!(
            rendered.ends_with(ORDINARY_END),
            "the colour is closed again: {rendered:?}"
        );
    }

    /// A heading is bold and blue: the palette's `#7aa2f7`, with the `# `
    /// marker interpreted away, and not the plain bold termimad would have
    /// painted.
    #[test]
    fn terminal_narration_styles_headings_bold_in_the_heading_colour() {
        let rendered = render_agent_text("# Heading", true);

        assert_eq!(
            rendered,
            format!("{HEADING}{BOLD}{UNDERLINE}Heading{STYLE_END}")
        );
        assert!(rendered.contains("Heading"), "heading text must survive");
        assert!(
            !rendered.contains("# Heading"),
            "the heading marker must be interpreted: {rendered:?}"
        );
        assert!(!rendered.starts_with(AGENT_PREFIX));

        let default = default_markdown("# Heading");
        assert_ne!(rendered, default, "the heading must not be the default one");
        assert!(
            !default.contains(HEADING),
            "the default skin paints no such blue: {default:?}"
        );
    }

    /// Heading levels stay apart under one heading colour: level one
    /// underlines, level two is plain bold, and level three and deeper add
    /// italic. termimad's own default collapses every level but the first into
    /// the same underlined style, so this is a level distinction the palette
    /// adds rather than one it preserves.
    #[test]
    fn terminal_narration_keeps_heading_levels_distinguishable() {
        let level_one = format!("{HEADING}{BOLD}{UNDERLINE}One{STYLE_END}");
        let level_two = format!("{HEADING}{BOLD}Two{STYLE_END}");
        let level_three = format!("{HEADING}{BOLD}{ITALIC}Three{STYLE_END}");
        let level_four = format!("{HEADING}{BOLD}{ITALIC}Four{STYLE_END}");

        let rendered = render_agent_text("# One\n\n## Two\n\n### Three\n\n#### Four", true);

        for heading in [&level_one, &level_two, &level_three, &level_four] {
            assert!(
                rendered.contains(heading.as_str()),
                "missing styled heading {heading:?} in {rendered:?}"
            );
        }
        assert_eq!(
            rendered.matches(HEADING).count(),
            4,
            "every level shares the heading colour: {rendered:?}"
        );

        assert_ne!(level_one, level_two, "levels one and two must differ");
        assert_ne!(level_two, level_three, "levels two and three must differ");
        assert!(
            level_one.contains(UNDERLINE) && !level_one.contains(ITALIC),
            "level one is the underlined one: {level_one:?}"
        );
        assert!(
            !level_two.contains(UNDERLINE) && !level_two.contains(ITALIC),
            "level two is plain bold: {level_two:?}"
        );
        assert!(
            level_three.contains(ITALIC) && !level_three.contains(UNDERLINE),
            "level three is the italic one: {level_three:?}"
        );
        let deepest_style = format!("{HEADING}{BOLD}{ITALIC}");
        assert!(
            level_three.starts_with(&deepest_style) && level_four.starts_with(&deepest_style),
            "levels three and deeper share the deepest pattern"
        );

        assert!(
            !rendered.contains("## "),
            "heading markers must be interpreted: {rendered:?}"
        );
    }

    /// `**strong**` is bold and `*emphasis*` is italic, both in the palette's
    /// magenta `#bb9af7`; between them, ordinary text goes back to the
    /// off-white. No Markdown delimiter survives.
    #[test]
    fn terminal_narration_styles_strong_and_italic_emphasis() {
        let rendered = render_agent_text("**bold** and *italic*", true);

        assert_eq!(
            rendered,
            format!(
                "{EMPHASIS}{BOLD}bold{STYLE_END}\
                 {ORDINARY} and {ORDINARY_END}\
                 {EMPHASIS}{ITALIC}italic{STYLE_END}"
            )
        );
        assert!(
            rendered.contains("bold") && rendered.contains("italic"),
            "both texts must stay readable: {rendered:?}"
        );
        assert!(
            !rendered.contains("**bold**") && !rendered.contains("*italic*"),
            "Markdown must be interpreted, not shown: {rendered:?}"
        );
        assert!(
            !rendered.contains(AGENT_PREFIX),
            "no marker on a terminal: {rendered:?}"
        );
    }

    /// Inline code keeps its text and takes the palette's yellow `#e0af68` --
    /// with no background, where termimad's default skin would have painted a
    /// grey one behind it.
    #[test]
    fn terminal_narration_styles_inline_code_without_a_background() {
        let rendered = render_agent_text("run `cargo test` now", true);

        assert_eq!(
            rendered,
            format!(
                "{ORDINARY}run {ORDINARY_END}\
                 {CODE}cargo test{ORDINARY_END}\
                 {ORDINARY} now{ORDINARY_END}"
            )
        );
        assert!(
            !rendered.contains("`cargo test`"),
            "the backticks must be interpreted: {rendered:?}"
        );

        let default = default_markdown("run `cargo test` now");
        assert_ne!(rendered, default, "the code must not be the default grey");
        assert!(
            default.contains(BACKGROUND_SET),
            "the default skin paints a background behind inline code: {default:?}"
        );
    }

    /// A narration that exercises every style the palette touches sets no
    /// background at all: not on inline code, not on a fenced block, and not
    /// on anything else.
    #[test]
    fn terminal_narration_sets_no_background() {
        let markdown = "# Heading\n\n\
                        Intro with `code`, **strong** and *emphasis*.\n\n\
                        - one\n\n\
                        ```\nfenced\n```";
        let rendered = render_agent_text(markdown, true);

        assert!(rendered.contains(&format!("{CODE}code{ORDINARY_END}")));
        assert!(rendered.contains(&format!("{CODE}fenced{ORDINARY_END}")));
        assert!(
            !rendered.contains(BACKGROUND_SET),
            "no background may be set: {rendered:?}"
        );
        assert!(
            !rendered.contains(BACKGROUND_RESET),
            "no background may be cleared either: {rendered:?}"
        );
    }

    /// The palette's cyan paints tool-call lines and termimad has no link
    /// element to paint with it: minimad 0.16, the parser behind termimad
    /// 0.35.5, parses no link syntax, and `MadSkin` exposes no link slot
    /// either. So a Markdown link arrives as the readable text it was written
    /// as, carried by the ordinary foreground -- the "where supported" half of
    /// the palette contract, pinned here so that a termimad upgrade which gains
    /// links shows up as a failing test instead of as silent drift.
    #[test]
    fn terminal_narration_keeps_links_readable_and_pins_the_palette_cyan() {
        assert_eq!(
            TOOL_CALL_FG,
            Color::Rgb {
                r: 0x7d,
                g: 0xcf,
                b: 0xff
            },
            "the palette's cyan is Tokyo Night `#7dcfff`"
        );

        let markdown = "see [label](https://example.com) now";
        let rendered = render_agent_text(markdown, true);

        assert_eq!(
            rendered,
            format!("{ORDINARY}see [label](https://example.com) now{ORDINARY_END}")
        );
        assert!(
            rendered.contains("label"),
            "the link's text must stay readable: {rendered:?}"
        );
        assert!(
            rendered.contains("https://example.com"),
            "the link's URL must stay readable: {rendered:?}"
        );
        assert!(
            !rendered.contains(CYAN),
            "no narration renders in the palette's cyan: {rendered:?}"
        );
        assert!(!rendered.contains(AGENT_PREFIX));
    }

    /// Headings, bullet lists and inline code are styled too: the original
    /// Markdown is interpreted, the block is the custom skin's own rendering,
    /// and none of the syntax appears literally.
    #[test]
    fn terminal_narration_renders_markdown_blocks() {
        let markdown = "# Heading\n\nIntro with `code`.\n\n- one\n- two";
        let rendered = render_agent_text(markdown, true);

        assert_eq!(
            rendered,
            styled_markdown(markdown),
            "the terminal branch renders through the canonical skin"
        );
        assert!(rendered.contains("Heading"), "heading text must survive");
        assert!(rendered.contains("one") && rendered.contains("two"));
        assert!(
            rendered.contains(&format!("{CODE}code{ORDINARY_END}")),
            "code keeps its text and takes the code colour: {rendered:?}"
        );
        assert!(!rendered.contains("`code`"), "code must be styled");
        assert!(!rendered.contains("# Heading"), "heading must be styled");
        assert!(
            !rendered.contains("- one"),
            "list markers must be interpreted"
        );
    }

    /// The terminal branch keeps the original narration and its embedded blank
    /// lines: markup-free text is passed through as it stands, with no `# `
    /// marker and no rewriting of the line structure.
    #[test]
    fn terminal_narration_preserves_embedded_blank_lines() {
        let markdown = "alpha\n\nbeta";
        let rendered = render_agent_text(markdown, true);

        assert_eq!(
            rendered,
            format!("{ORDINARY}alpha{ORDINARY_END}\n\n{ORDINARY}beta{ORDINARY_END}")
        );
        assert_eq!(rendered, styled_markdown(markdown));
        assert_eq!(
            rendered.matches("\n\n").count(),
            1,
            "the blank line survives: {rendered:?}"
        );
        assert!(!rendered.contains(AGENT_PREFIX));
    }

    /// The terminal branch is not termimad's default rendering: the custom
    /// skin's colours replace the default ones on every element the palette
    /// names, and the default skin's inline-code background is gone.
    #[test]
    fn terminal_narration_differs_from_termimads_default_skin() {
        let markdown = "# Heading\n\nIntro with `code` and **strong** and *emphasis*.";
        let rendered = render_agent_text(markdown, true);
        let default = default_markdown(markdown);

        assert_ne!(
            rendered, default,
            "the custom skin must not render like the default one"
        );
        assert!(
            rendered.contains(HEADING) && rendered.contains(CODE) && rendered.contains(EMPHASIS),
            "the palette reaches every element in the sample: {rendered:?}"
        );
        assert!(
            !rendered.contains(BACKGROUND_SET),
            "and paints no background: {rendered:?}"
        );
        assert!(
            !default.contains(HEADING) && !default.contains(CODE),
            "the default rendering is the one without the palette: {default:?}"
        );
    }

    /// The renderer adds no line terminator of its own, on either branch:
    /// termimad's block newline is dropped so the caller's `println!` ends the
    /// line exactly once, just as it does for plain narration.
    #[test]
    fn neither_branch_ends_with_a_line_terminator_of_its_own() {
        for content in ["alpha", "alpha\n\nbeta", "# Heading\n\nbody"] {
            let rendered = render_agent_text(content, true);
            assert!(
                !rendered.ends_with('\n'),
                "terminal narration must not end in a newline: {rendered:?}"
            );
            // The dropped terminator is precisely termimad's own block
            // terminator, so `println!` restores the original line structure --
            // rendered with the custom skin, as the terminal branch does.
            assert_eq!(
                format!("{rendered}\n"),
                tokyo_night_skin().text(content, None).to_string(),
                "the rendered block must be the custom skin's, terminator aside"
            );
        }

        for content in ["alpha", "alpha\n\nbeta", "# Heading\n\nbody"] {
            assert!(
                !render_agent_text(content, false).ends_with('\n'),
                "plain narration must not add a newline of its own"
            );
        }

        assert_eq!(
            render_agent_text("", true),
            "",
            "an empty terminal input renders to nothing"
        );
        assert_eq!(render_agent_text("", false), "");
    }

    /// The two branches really are different contracts for the same content:
    /// the plain one carries the marker and no escapes, the terminal one
    /// carries styling and no marker.
    #[test]
    fn the_branches_select_different_contracts() {
        let markdown = "**bold** narration";
        let plain = render_agent_text(markdown, false);
        let terminal = render_agent_text(markdown, true);

        assert_eq!(plain, "# **bold** narration");
        assert!(!plain.contains('\u{1b}'));
        assert!(terminal.contains('\u{1b}'));
        assert!(!terminal.starts_with(AGENT_PREFIX));
        assert_ne!(plain, terminal);
    }

    // -- Provider-role in-flight status -------------------------------------

    /// Core entry-point behavior: the transient label leads with the canonical
    /// lowercase name of the exact phase being sent, immediately followed by
    /// `...`, so the first thing on the row is the pass the request serves.
    /// Every closed phase variant is covered, and the generic `thinking...`
    /// prefix is gone.
    #[test]
    fn when_request_status_is_shown_given_each_active_phase_then_uses_its_canonical_prefix() {
        for phase in PHASES {
            let label = thinking_label(phase, "mid", 12_345, None);
            assert_eq!(
                label,
                format!("{}... [mid] (12,345 B ~ 3,086 tokens)", phase.label()),
                "phase {phase:?} must lead with its own canonical label"
            );
            assert!(
                !label.starts_with("thinking"),
                "the generic transient prefix must be gone: {label}"
            );
        }

        assert_eq!(
            thinking_label(Phase::Code, "mid", 12_345, None),
            "code... [mid] (12,345 B ~ 3,086 tokens)"
        );
        assert_eq!(
            thinking_label(Phase::Plan, "giga", 0, None),
            "plan... [giga] (0 B ~ 0 tokens)"
        );
    }

    /// Empty/zero boundary: the smallest context still reports a bare zero for
    /// bytes and tokens, behind every canonical phase's own prefix and for both
    /// canonical roles.
    #[test]
    fn when_request_status_is_shown_given_zero_context_then_shows_zero_bytes_and_tokens() {
        for phase in PHASES {
            for role in ROLES {
                assert_eq!(
                    thinking_label(phase, role, 0, None),
                    format!("{}... [{role}] (0 B ~ 0 tokens)", phase.label())
                );
            }
        }
    }

    /// Minimum valid boundary: any context below `BYTES_PER_TOKEN` yields a
    /// token figure of zero -- the division is floored, never rounded up -- and
    /// the byte figure is kept exactly, for both canonical roles.
    #[test]
    fn when_request_status_is_shown_given_sub_token_context_then_keeps_bytes_and_floors_tokens() {
        for role in ROLES {
            assert_eq!(
                thinking_label(Phase::Code, role, 1, None),
                format!("code... [{role}] (1 B ~ 0 tokens)")
            );
            assert_eq!(
                thinking_label(Phase::Code, role, 3, None),
                format!("code... [{role}] (3 B ~ 0 tokens)")
            );
        }
    }

    /// Thousands are grouped with `,` every three digits from the right, in
    /// both the byte figure and the approximate token figure, whatever the
    /// role -- the role and the phase prefix never consume part of a figure.
    #[test]
    fn when_request_status_is_shown_given_grouped_context_then_groups_bytes_and_tokens() {
        for role in ROLES {
            assert_eq!(
                thinking_label(Phase::Code, role, 1234, None),
                format!("code... [{role}] (1,234 B ~ 308 tokens)")
            );
            assert_eq!(
                thinking_label(Phase::Code, role, 1234567, None),
                format!("code... [{role}] (1,234,567 B ~ 308,641 tokens)")
            );
        }
    }

    /// A realistic context renders both grouped figures complete, is not
    /// truncated (no trailing ellipsis) and stays inside the one-row budget --
    /// for every phase and both canonical roles.
    #[test]
    fn when_request_status_is_shown_given_realistic_context_then_keeps_complete_figures() {
        for phase in PHASES {
            for role in ROLES {
                let label = thinking_label(phase, role, 1_000_000, None);
                assert_eq!(
                    label,
                    format!(
                        "{}... [{role}] (1,000,000 B ~ 250,000 tokens)",
                        phase.label()
                    )
                );
                assert!(!label.ends_with(ELLIPSIS), "unexpected ellipsis: {label}");
                assert!(
                    label.chars().count() < MAX_LABEL,
                    "a realistic context must fit one row comfortably: {label}"
                );
            }
        }
    }

    /// Maximum/upper boundary: the largest `usize` is capped at `MAX_LABEL`
    /// characters with the trailing truncation marker, for every canonical
    /// phase and both roles, and still names the phase and the role.
    #[test]
    fn when_request_status_is_shown_given_maximum_context_then_caps_every_phase_and_role_with_ellipsis()
     {
        for phase in PHASES {
            for role in ROLES {
                let widest = thinking_label(phase, role, usize::MAX, None);
                assert_eq!(widest.chars().count(), MAX_LABEL);
                assert!(
                    widest.starts_with(&format!("{}... [{role}] (18,", phase.label())),
                    "bad start: {widest}"
                );
                assert!(
                    widest.contains(&format!("[{role}]")),
                    "role missing from the capped label: {widest}"
                );
                assert!(widest.ends_with(ELLIPSIS), "missing ellipsis: {widest}");
            }
        }

        assert_eq!(
            thinking_label(Phase::Code, "mid", usize::MAX, None),
            "code... [mid] (18,446,744,073,709,551,615 B ~ 4,611,686,0..."
        );
        assert_eq!(
            thinking_label(Phase::Review, "giga", usize::MAX, None),
            "review... [giga] (18,446,744,073,709,551,615 B ~ 4,611,68..."
        );

        assert_eq!(MAX_LABEL, 60);
    }

    /// Contract & invariant: the label keeps one canonical shape --
    /// `<phase>... [<role>] (<bytes> B ~ <tokens>)` -- with exactly one phase
    /// prefix, exactly one role occurrence, the exact ` B ~ ` and ` tokens)`
    /// delimiters, no separator abutting a bracket, and no generic transient
    /// prefix.
    #[test]
    fn when_request_status_is_shown_given_phase_role_and_context_then_keeps_canonical_delimiters() {
        for phase in PHASES {
            for role in ROLES {
                let prefix = format!("{}... [{role}] (", phase.label());
                for value in [0, 1000, 1000000] {
                    let label = thinking_label(phase, role, value, None);
                    assert!(label.starts_with(&prefix), "bad prefix: {label}");
                    assert!(label.ends_with(" tokens)"), "bad suffix: {label}");
                    assert!(label.contains(" B ~ "), "missing figure separator: {label}");
                    assert_eq!(
                        label.matches(role).count(),
                        1,
                        "role not rendered exactly once: {label}"
                    );
                    assert_eq!(
                        label.matches(&format!("{}...", phase.label())).count(),
                        1,
                        "phase prefix not rendered exactly once: {label}"
                    );
                    assert!(!label.contains("thinking"), "generic prefix: {label}");

                    let bytes = &label[prefix.len()..label.find(" B").unwrap()];
                    assert!(
                        !bytes.is_empty() && bytes.chars().all(|c| c.is_ascii_digit() || c == ','),
                        "byte figure does not precede ` B`: {label}"
                    );
                    assert!(!bytes.starts_with(','), "leading separator: {label}");
                    assert!(!bytes.ends_with(','), "trailing separator: {label}");

                    assert!(!label.contains("(,"), "separator after `(`: {label}");
                    assert!(
                        !label.contains("( ~"),
                        "figure separator after `(`: {label}"
                    );
                    assert!(
                        !label.contains(", tokens)"),
                        "separator before suffix: {label}"
                    );
                    assert!(
                        !label.contains("~ tokens)"),
                        "figure separator before suffix: {label}"
                    );
                }
            }
        }
    }

    // -- Priced in-flight status --------------------------------------------

    /// Core entry-point behavior: the canonical formatter accepts the selected
    /// per-million input rate, keeps the existing phase, role, bytes and tokens
    /// and adds the exact USD input estimate rendered from the displayed token
    /// count -- for every closed phase and both canonical roles.
    #[test]
    fn when_formatting_a_running_label_given_a_configured_rate_then_uses_the_canonical_label_contract()
     {
        assert_eq!(
            thinking_label(Phase::Code, "mid", 12_345, Some(2.5)),
            "code... [mid] (12,345 B ~ 3,086 tokens ~ $0.007715 input)"
        );

        for phase in PHASES {
            for role in ROLES {
                assert_eq!(
                    thinking_label(phase, role, 400, Some(1.5)),
                    format!(
                        "{}... [{role}] (400 B ~ 100 tokens ~ $0.000150 input)",
                        phase.label()
                    ),
                    "phase {phase:?} served by {role} must price through the canonical label"
                );
            }
        }
    }

    /// Empty/zero boundary: a missing rate is not a zero rate, so the canonical
    /// label keeps the legacy shape with no placeholder, currency symbol or
    /// estimate.
    #[test]
    fn when_formatting_a_running_label_given_no_rate_then_preserves_the_legacy_label() {
        for phase in PHASES {
            for role in ROLES {
                assert_eq!(
                    thinking_label(phase, role, 12_345, None),
                    format!("{}... [{role}] (12,345 B ~ 3,086 tokens)", phase.label())
                );
            }
        }
    }

    /// Empty/zero boundary: a configured rate of zero is present, so it renders
    /// a zero estimate instead of disappearing like a missing rate.
    #[test]
    fn when_formatting_a_running_label_given_zero_rate_then_keeps_zero_distinct_from_missing() {
        assert_eq!(
            thinking_label(Phase::Code, "mid", 12_345, Some(0.0)),
            "code... [mid] (12,345 B ~ 3,086 tokens ~ $0.000000 input)"
        );
        assert_ne!(
            thinking_label(Phase::Code, "mid", 12_345, Some(0.0)),
            thinking_label(Phase::Code, "mid", 12_345, None),
            "a configured zero rate must not render like a missing rate"
        );
    }

    /// Empty/zero boundary: a zero-byte context with a configured input rate
    /// still renders every figure, so the approximate token count and the input
    /// cost are both exactly zero, while the same zero bytes without a rate
    /// preserve the prior unpriced label.
    #[test]
    fn when_formatting_a_running_label_given_zero_bytes_and_a_configured_rate_then_shows_zero_input_cost()
     {
        let label = thinking_label(Phase::Code, "mid", 0, Some(2.5));
        assert_eq!(label, "code... [mid] (0 B ~ 0 tokens ~ $0.000000 input)");
        assert!(
            label.contains("0 B ~ 0 tokens ~ $0.000000 input"),
            "the zero-byte label must carry zero figures and zero cost: {label}"
        );

        for phase in PHASES {
            for role in ROLES {
                assert_eq!(
                    thinking_label(phase, role, 0, Some(2.5)),
                    format!(
                        "{}... [{role}] (0 B ~ 0 tokens ~ $0.000000 input)",
                        phase.label()
                    ),
                    "phase {phase:?} served by {role} must price zero bytes at zero"
                );
            }
        }

        assert_eq!(
            thinking_label(Phase::Code, "mid", 0, None),
            "code... [mid] (0 B ~ 0 tokens)",
            "a missing rate must preserve the prior zero-byte label"
        );
    }

    /// Minimum valid boundary: one byte is below the tokens-per-byte heuristic,
    /// so the displayed approximate token count and the cost are both floored
    /// to zero while the byte figure remains one -- through the canonical
    /// signature, priced and unpriced alike.
    #[test]
    fn when_formatting_a_running_label_given_one_byte_then_keeps_floor_division_with_the_canonical_signature()
     {
        assert_eq!(
            thinking_label(Phase::Code, "mid", 1, Some(2.5)),
            "code... [mid] (1 B ~ 0 tokens ~ $0.000000 input)"
        );
        assert_eq!(
            thinking_label(Phase::Code, "mid", 1, None),
            "code... [mid] (1 B ~ 0 tokens)"
        );
        for role in ROLES {
            assert_eq!(
                thinking_label(Phase::Code, role, 3, Some(2.5)),
                format!("code... [{role}] (3 B ~ 0 tokens ~ $0.000000 input)")
            );
        }
    }

    /// Maximum/upper boundary: the price suffix is added before the existing
    /// cap, so the widest context still becomes the same sixty-character label
    /// with the leading phase and role intact and the trailing ellipsis.
    #[test]
    fn when_formatting_a_running_label_given_usize_max_context_then_keeps_the_bounded_ellipsis() {
        for phase in PHASES {
            for role in ROLES {
                let widest = thinking_label(phase, role, usize::MAX, Some(2.5));
                assert_eq!(widest.chars().count(), MAX_LABEL);
                assert!(
                    widest.starts_with(&format!("{}... [{role}] (18,", phase.label())),
                    "bad start: {widest}"
                );
                assert!(
                    widest.contains(&format!("[{role}]")),
                    "role missing from the capped label: {widest}"
                );
                assert!(widest.ends_with(ELLIPSIS), "missing ellipsis: {widest}");
                assert_eq!(
                    widest,
                    thinking_label(phase, role, usize::MAX, None),
                    "the cap must keep the leading identity regardless of the rate"
                );
            }
        }

        assert_eq!(
            thinking_label(Phase::Code, "mid", usize::MAX, Some(2.5)),
            "code... [mid] (18,446,744,073,709,551,615 B ~ 4,611,686,0..."
        );
    }

    /// Malformed/invalid payload: a finite configured rate is rendered as a
    /// fixed six-decimal USD amount followed by the input-only suffix; the raw
    /// rate is never shown, and providers reject invalid rates upstream.
    #[test]
    fn when_formatting_a_running_label_given_a_finite_rate_then_formats_usd_not_a_raw_rate() {
        let label = thinking_label(Phase::Code, "mid", 12_345, Some(2.5));

        assert!(label.ends_with("~ $0.007715 input)"), "bad suffix: {label}");
        assert!(
            label.contains(" ~ $0.007715 input)"),
            "the estimate needs its own separator: {label}"
        );
        assert!(!label.contains("2.5"), "raw rate leaked: {label}");
        assert!(
            !label.contains("2.500000"),
            "the rate is not the estimate: {label}"
        );
        assert_eq!(
            label.matches('$').count(),
            1,
            "exactly one USD amount: {label}"
        );
        assert_eq!(
            label.matches(" input)").count(),
            1,
            "input-only suffix rendered once: {label}"
        );
        let amount = label
            .split("~ $")
            .nth(1)
            .and_then(|rest| rest.split(" input)").next())
            .unwrap_or_default();
        assert_eq!(
            amount.split('.').nth(1).map(str::len),
            Some(6),
            "the USD amount needs six fractional digits: {label}"
        );
    }

    /// Regression: the no-rate branch is byte-for-byte the pre-feature label,
    /// so migrating the signature leaves the existing no-price contract
    /// untouched.
    #[test]
    fn when_formatting_a_running_label_given_no_rate_then_matches_the_existing_label_byte_for_byte()
    {
        for phase in PHASES {
            for role in ROLES {
                assert_eq!(
                    thinking_label(phase, role, 12_345, None),
                    format!("{}... [{role}] (12,345 B ~ 3,086 tokens)", phase.label()),
                    "no-rate label changed for {phase:?}/{role}"
                );
            }
        }

        for (context_bytes, expected) in [
            (0, "code... [mid] (0 B ~ 0 tokens)"),
            (1, "code... [mid] (1 B ~ 0 tokens)"),
            (3, "code... [mid] (3 B ~ 0 tokens)"),
            (1_234, "code... [mid] (1,234 B ~ 308 tokens)"),
            (12_345, "code... [mid] (12,345 B ~ 3,086 tokens)"),
            (1_000_000, "code... [mid] (1,000,000 B ~ 250,000 tokens)"),
            (
                usize::MAX,
                "code... [mid] (18,446,744,073,709,551,615 B ~ 4,611,686,0...",
            ),
        ] {
            assert_eq!(
                thinking_label(Phase::Code, "mid", context_bytes, None),
                expected,
                "the pre-feature label changed at {context_bytes} bytes"
            );
        }
    }

    /// Contract & invariant: the pure label carries only the phase, role and
    /// context figures plus the estimate -- no credentials and no
    /// provider-reported usage could have reached it.
    #[test]
    fn when_formatting_a_running_label_given_a_rate_then_exposes_no_credentials() {
        let label = thinking_label(Phase::Code, "mid", 12_345, Some(2.5));

        for forbidden in [
            "sk-",
            "Bearer",
            "Authorization",
            "api_key",
            "API_KEY",
            "prompt_tokens",
            "completion_tokens",
            "total_tokens",
            "provider_usage",
            "usage",
            "secret",
        ] {
            assert!(
                !label.contains(forbidden),
                "the label must not expose {forbidden}: {label}"
            );
        }
    }

    /// Thousands stay grouped in every figure: the byte count, the displayed
    /// token count and the estimate all come from the same grouped context.
    #[test]
    fn when_formatting_a_running_label_given_grouped_context_then_groups_bytes_tokens_and_estimate()
    {
        for role in ROLES {
            assert_eq!(
                thinking_label(Phase::Code, role, 99_999, Some(1.0)),
                format!("code... [{role}] (99,999 B ~ 24,999 tokens ~ $0.024999 input)")
            );
        }
    }

    /// The separator only appears once a fourth digit is present.
    #[test]
    fn when_grouping_a_figure_given_three_or_four_digits_then_adds_a_separator_only_at_four() {
        assert_eq!(group_digits(999), "999");
        assert_eq!(group_digits(1000), "1,000");
    }

    // -- Log severity rendering ---------------------------------------------

    /// The three recognized tags take their conventional foregrounds, and the
    /// codes really are 36, 33 and 31 -- not the 256-colour selections
    /// crossterm writes for its own basic colours.
    #[test]
    fn severity_tags_use_the_conventional_foreground_codes() {
        assert_eq!(INFO_FG, "\u{1b}[36m");
        assert_eq!(WARN_FG, "\u{1b}[33m");
        assert_eq!(ERROR_FG, "\u{1b}[31m");
        assert_eq!(SEVERITY_TAGS.len(), 3);
    }

    /// Each recognized tag is coloured in its conventional code, the full reset
    /// lands immediately after the tag, and the remainder -- whitespace
    /// included -- is untouched.
    #[test]
    fn a_terminal_log_colours_only_the_recognized_tag() {
        assert_eq!(
            render_log("[INFO] ready", true),
            format!("{INFO_FG}[INFO]{STYLE_END} ready")
        );
        assert_eq!(
            render_log("[WARN] slow", true),
            format!("{WARN_FG}[WARN]{STYLE_END} slow")
        );
        assert_eq!(
            render_log("[ERROR] boom", true),
            format!("{ERROR_FG}[ERROR]{STYLE_END} boom")
        );

        let rendered = render_log("[INFO] ready", true);
        assert_eq!(rendered.matches(INFO_FG).count(), 1);
        assert_eq!(rendered.matches(STYLE_END).count(), 1);
        assert!(rendered.starts_with(INFO_FG));
        assert!(
            rendered.find(STYLE_END).unwrap() > rendered.find("[INFO]").unwrap(),
            "the reset lands after the marker: {rendered:?}"
        );
    }

    /// A tag is recognized whether or not whitespace follows it: the marker is
    /// all that is coloured, with or without a message behind it.
    #[test]
    fn a_tag_is_recognized_with_or_without_following_whitespace() {
        assert_eq!(
            render_log("[INFO]", true),
            format!("{INFO_FG}[INFO]{STYLE_END}")
        );
        assert_eq!(
            render_log("[WARN]", true),
            format!("{WARN_FG}[WARN]{STYLE_END}")
        );
        assert_eq!(
            render_log("[ERROR]", true),
            format!("{ERROR_FG}[ERROR]{STYLE_END}")
        );
        assert_eq!(
            render_log("[INFO]x", true),
            format!("{INFO_FG}[INFO]{STYLE_END}x")
        );
        assert_eq!(
            render_log("[WARN]  ", true),
            format!("{WARN_FG}[WARN]{STYLE_END}  ")
        );
    }

    /// The message remainder survives byte for byte: leading and trailing
    /// spaces, tabs and embedded newlines included, with the colour closed
    /// before any of it.
    #[test]
    fn a_coloured_log_preserves_the_remainder_verbatim() {
        assert_eq!(
            render_log("[ERROR] first line\nsecond line\n\nfourth line", true),
            format!("{ERROR_FG}[ERROR]{STYLE_END} first line\nsecond line\n\nfourth line")
        );
        assert_eq!(
            render_log("[INFO]   spaced \t text  ", true),
            format!("{INFO_FG}[INFO]{STYLE_END}   spaced \t text  ")
        );
        assert_eq!(
            render_log("[WARN] already\nterminated\n", true),
            format!("{WARN_FG}[WARN]{STYLE_END} already\nterminated\n")
        );
    }

    /// A payload with no recognized leading tag is returned exactly as handed
    /// over, escape-free: absent, unrecognized, differently cased, mis-bracketed
    /// and non-leading tags alike.
    #[test]
    fn an_unrecognized_log_is_returned_unchanged() {
        let cases = [
            "ready",
            "",
            "[DEBUG] verbose",
            "[TRACE] verbose",
            "[info] quiet",
            "[Warn] mixed",
            "[error]",
            "[ERROR ] trailing space",
            "[INFOX] almost",
            "see [INFO] not leading",
            " [INFO] leading space",
            "\n[INFO] leading newline",
            "[] [INFO]",
            "# [INFO]",
        ];
        for payload in cases {
            let rendered = render_log(payload, true);
            assert_eq!(rendered, payload, "an unrecognized payload changed");
            assert!(
                !rendered.contains('\u{1b}'),
                "an unrecognized payload carries no ANSI: {rendered:?}"
            );
        }
    }

    /// The non-terminal branch is the payload byte for byte, tagged or not.
    #[test]
    fn a_non_terminal_log_is_the_plain_payload() {
        for payload in [
            "[INFO] ready",
            "[WARN] slow",
            "[ERROR] boom",
            "[DEBUG] verbose",
            "ready",
            "[ERROR] first\nsecond\n",
        ] {
            let rendered = render_log(payload, false);
            assert_eq!(rendered, payload, "a redirected log changed");
            assert!(
                !rendered.contains('\u{1b}'),
                "a redirected log carries no ANSI: {rendered:?}"
            );
        }
    }

    /// The two branches are two contracts for the same line: the terminal one
    /// adds the escapes around the marker, and an unrecognized payload is
    /// identical on both.
    #[test]
    fn the_log_branches_differ_only_by_the_marker_colour() {
        let terminal = render_log("[WARN] slow", true);
        let plain = render_log("[WARN] slow", false);

        assert_eq!(plain, "[WARN] slow");
        assert_eq!(terminal, format!("{WARN_FG}[WARN]{STYLE_END} slow"));
        assert_ne!(plain, terminal);
        assert_eq!(
            terminal[WARN_FG.len()..],
            format!("[WARN]{STYLE_END} slow"),
            "only escapes precede the marker"
        );

        for payload in ["[DEBUG] x", "ready", ""] {
            assert_eq!(render_log(payload, true), render_log(payload, false));
        }
    }

    // -- Tool-call rendering ------------------------------------------------

    /// On a terminal the whole payload is the palette's cyan, and the colour is
    /// closed by a full reset (crossterm's `\x1b[0m`) immediately after the last
    /// character of the payload -- before the newline any caller adds. The `$ `
    /// marker and the target are still there, inside the colour.
    #[test]
    fn a_terminal_tool_call_is_the_whole_payload_in_cyan_and_reset() {
        assert_eq!(
            render_tool_call("$ ls -la", true),
            format!("{CYAN}$ ls -la{STYLE_END}")
        );
        assert_eq!(
            render_tool_call("$ echo ejaj-ok", true),
            format!("{CYAN}$ echo ejaj-ok{STYLE_END}")
        );

        let rendered = render_tool_call("$ echo ejaj-ok", true);
        assert!(
            rendered.starts_with(CYAN),
            "the payload opens in the palette's cyan: {rendered:?}"
        );
        assert!(
            rendered.contains("$ "),
            "the `$ ` marker survives: {rendered:?}"
        );
        assert!(
            rendered.contains("echo ejaj-ok"),
            "the target survives: {rendered:?}"
        );
        assert!(
            rendered.ends_with(STYLE_END),
            "the full reset closes the payload: {rendered:?}"
        );
        assert_eq!(
            rendered.matches(CYAN).count(),
            1,
            "the foreground is selected once: {rendered:?}"
        );
        assert_eq!(
            rendered.matches(STYLE_END).count(),
            1,
            "the reset is written once: {rendered:?}"
        );
        assert!(
            !rendered.contains('\n'),
            "the renderer adds no line terminator: {rendered:?}"
        );
        assert_eq!(
            STYLE_END, "\u{1b}[0m",
            "the reset after the payload is the full one"
        );
    }

    /// A redirected stdout gets the payload byte for byte: no escape sequence
    /// and no rewriting, whatever the line looks like.
    #[test]
    fn a_non_terminal_tool_call_is_the_plain_payload() {
        for payload in [
            "$ ls -la",
            "$",
            "$ echo 'hi'",
            "$ cargo test -- --test-threads=1",
        ] {
            let rendered = render_tool_call(payload, false);
            assert_eq!(rendered, payload, "the plain branch changed the payload");
            assert!(
                !rendered.contains('\u{1b}'),
                "a redirected call line carries no ANSI: {rendered:?}"
            );
        }
    }

    /// The two branches are two contracts for the same line, and the plain one
    /// is the line the call formatter builds: the colour is added around the
    /// payload, never into it, and the call line itself stays plain.
    #[test]
    fn when_tool_call_is_rendered_given_terminal_and_redirected_stdout_then_differs_only_by_colour()
    {
        let line = call_line(1, 100, "cargo test", None);
        let plain = render_tool_call(&line, false);
        let terminal = render_tool_call(&line, true);

        assert_eq!(line, "[1/100] $ cargo test");
        assert_eq!(plain, line, "the plain branch is the call line's own bytes");
        assert_eq!(terminal, format!("{CYAN}{line}{STYLE_END}"));
        assert_ne!(plain, terminal, "the two branches must differ");
        assert_eq!(
            terminal[line.len()..].len(),
            format!("{CYAN}{STYLE_END}").len(),
            "only escapes are added around the payload: {terminal:?}"
        );
        assert!(
            !line.contains('\u{1b}'),
            "the call formatter never colours its own result: {line:?}"
        );
    }

    /// Every executed call's time is whole seconds plus exactly three floored
    /// decimal milliseconds, with the unit glued to the figure: zero and every
    /// sub-millisecond duration floor to `0.000s`, the first whole millisecond
    /// is `0.001s`, and no minute or hour ever changes the seconds figure.
    #[test]
    fn when_command_duration_is_formatted_given_boundary_durations_then_floors_milliseconds() {
        assert_eq!(call_time(Duration::ZERO), "0.000s");
        assert_eq!(call_time(Duration::from_micros(999)), "0.000s");
        assert_eq!(call_time(Duration::from_millis(1)), "0.001s");
        assert_eq!(
            call_time(Duration::from_secs(3) + Duration::from_millis(477)),
            "3.477s"
        );
        assert_eq!(
            call_time(Duration::from_secs(61) + Duration::from_millis(500)),
            "61.500s",
            "a minute is still seconds, never a new unit"
        );
        assert_eq!(
            call_time(Duration::from_secs(12_345) + Duration::from_millis(678)),
            "12345.678s",
            "a long duration keeps every whole second and no grouping"
        );

        for elapsed in [
            Duration::ZERO,
            Duration::from_nanos(1),
            Duration::from_micros(999),
            Duration::from_millis(1),
            Duration::from_secs(12_345) + Duration::from_millis(678),
        ] {
            let formatted = call_time(elapsed);
            let (seconds, fraction) = formatted
                .strip_suffix('s')
                .and_then(|value| value.split_once('.'))
                .unwrap_or_else(|| panic!("not a call time: {formatted:?}"));
            assert!(
                !seconds.is_empty() && seconds.chars().all(|c| c.is_ascii_digit()),
                "whole seconds must be plain digits: {formatted:?}"
            );
            assert_eq!(
                fraction.len(),
                3,
                "the fraction is exactly three digits: {formatted:?}"
            );
            assert!(
                fraction.chars().all(|c| c.is_ascii_digit()),
                "the fraction must be ASCII digits: {formatted:?}"
            );
        }
    }

    /// The call line is one-based: step 1 is `1/100` and the last step is
    /// `100/100`, on both an executed and a rejected call.
    #[test]
    fn when_call_echo_is_rendered_given_first_and_last_steps_then_uses_one_based_counter() {
        assert_eq!(
            call_line(1, 100, "echo first", Some(Duration::from_millis(1))),
            "[1/100] (0.001s) $ echo first"
        );
        assert_eq!(
            call_line(100, 100, "echo last", None),
            "[100/100] $ echo last"
        );
    }

    /// Minimum valid boundary: one millisecond carries a zero-padded
    /// three-digit fraction, inline, on the first step's call line.
    #[test]
    fn when_command_duration_is_formatted_given_one_millisecond_then_shows_three_digits() {
        assert_eq!(call_time(Duration::from_millis(1)), "0.001s");
        assert_eq!(
            call_line(1, 100, "echo hi", Some(Duration::from_millis(1))),
            "[1/100] (0.001s) $ echo hi"
        );
    }

    /// A long or multi-byte call is sanitized to one row and capped as a whole
    /// line: the step counter and inline duration are part of the 100-character
    /// budget, and Unicode is cut between characters with a trailing ellipsis.
    #[test]
    fn when_long_call_is_rendered_given_unicode_command_then_stays_capped_safely() {
        let multiline = "js:\nconst s = tools.read(\"src/tools.rs\", 1, 40);\nreturn s;";
        assert_eq!(
            call_line(1, 100, multiline, None),
            "[1/100] $ js: const s = tools.read(\"src/tools.rs\", 1, 40); return s;"
        );
        assert_eq!(
            call_line(1, 100, "a\tb\rc", None),
            "[1/100] $ a b c",
            "control characters collapse to single spaces"
        );

        let long = "界".repeat(120);
        let capped = call_line(1, 100, &long, Some(Duration::ZERO));
        assert_eq!(
            capped.chars().count(),
            MAX_CALL_LINE,
            "the complete call line is capped, not just the display"
        );
        assert!(capped.starts_with("[1/100] (0.000s) $ 界"));
        assert!(capped.ends_with("..."));
        assert_eq!(
            capped.matches('界').count(),
            100 - "[1/100] (0.000s) $ ".chars().count() - 3
        );
    }

    /// Maximum/upper boundary: step 100 and a duration past three hours keep
    /// the full one-hundreds counter and every second, and the whole line is
    /// still capped Unicode-safely.
    #[test]
    fn when_call_is_rendered_given_step_one_hundred_and_long_duration_then_preserves_full_counter_seconds_and_cap()
     {
        let long_elapsed = Duration::from_secs(12_345) + Duration::from_millis(678);
        assert_eq!(
            call_line(100, 100, "echo done", Some(long_elapsed)),
            "[100/100] (12345.678s) $ echo done"
        );

        let long = "界".repeat(200);
        let capped = call_line(100, 100, &long, Some(long_elapsed));
        assert_eq!(capped.chars().count(), MAX_CALL_LINE);
        assert!(capped.starts_with("[100/100] (12345.678s) $ 界"));
        assert!(capped.ends_with("..."));
    }

    // -- Compact thinking reports -------------------------------------------

    /// Zero/empty case: a zero or sub-millisecond duration floors to the same
    /// three-digit zero figure, and a zero context is a bare `0 B`. The model
    /// and context are supplied metadata, never synthesized here.
    #[test]
    fn when_thinking_report_is_formatted_given_zero_values_then_shows_zero_duration_and_context() {
        for elapsed in [
            Duration::ZERO,
            Duration::from_nanos(1),
            Duration::from_micros(999),
        ] {
            assert_eq!(
                thinking_report("mid", "example-model", "code", 0, elapsed),
                "#thinking (0.000s) mid model=example-model phase=code context=0 B"
            );
        }

        assert_eq!(
            thinking_report("smoll", "smoll-model", "summary", 0, Duration::ZERO),
            "#thinking (0.000s) smoll model=smoll-model phase=summary context=0 B"
        );
    }

    /// Minimum valid boundary: the first whole millisecond keeps its
    /// zero-padded fraction and one byte stays a bare figure.
    #[test]
    fn when_thinking_report_is_formatted_given_minimum_values_then_shows_one_millisecond_and_one_byte()
     {
        assert_eq!(
            thinking_report("giga", "giga-model", "plan", 1, Duration::from_millis(1)),
            "#thinking (0.001s) giga model=giga-model phase=plan context=1 B"
        );
    }

    /// Maximum/upper boundary: the largest context is grouped without
    /// truncation, and a long duration keeps ungrouped whole seconds past
    /// minutes and hours.
    #[test]
    fn when_thinking_report_is_formatted_given_maximum_values_then_preserves_grouping_and_whole_seconds()
     {
        assert_eq!(
            thinking_report(
                "mid",
                "mid-model",
                "code",
                usize::MAX,
                Duration::from_secs(12_345) + Duration::from_millis(678)
            ),
            "#thinking (12345.678s) mid model=mid-model phase=code context=18,446,744,073,709,551,615 B"
        );

        assert_eq!(
            thinking_report(
                "giga",
                "giga-model",
                "review",
                1_000_000,
                Duration::from_millis(61_500)
            ),
            "#thinking (61.500s) giga model=giga-model phase=review context=1,000,000 B"
        );
        assert_eq!(
            thinking_report(
                "smoll",
                "smoll-model",
                "summary",
                999,
                Duration::from_secs(3_600)
            ),
            "#thinking (3600.000s) smoll model=smoll-model phase=summary context=999 B"
        );
    }

    /// Regression: the completed-attempt report stays a separate line with its
    /// established payload. The transient label names the phase now, and that
    /// change must not reach this report: the exact mid / example-model /
    /// code / 12,345 B / 1.234s metadata still renders unchanged, as a stdout
    /// log with one terminator and no transient prefix.
    #[test]
    fn when_a_completed_attempt_is_reported_given_mid_code_metadata_then_preserves_exact_payload() {
        let payload = "#thinking (1.234s) mid model=example-model phase=code context=12,345 B";
        assert_eq!(
            thinking_report(
                "mid",
                "example-model",
                "code",
                12_345,
                Duration::from_millis(1_234)
            ),
            payload
        );

        let mut out = OutputCoordinator::new(RecordingSink::new());
        out.thinking_log(
            "mid",
            "example-model",
            "code",
            12_345,
            Duration::from_millis(1_234),
        )
        .unwrap();
        assert_eq!(out.sink().stdout(), format!("{payload}\n"));
        assert_eq!(out.sink().stderr(), "", "the report is a stdout log");
        assert_eq!(out.last(), Some(OutputKind::Log));
    }

    /// A report is a stdout log with the coordinator's ordinary framing: the
    /// exact payload, one newline, the same-kind spacing rule and a flush --
    /// and no severity colour, because `#thinking` is not a recognized tag.
    /// On a terminal the complete payload takes the palette's cyan instead.
    #[test]
    fn when_model_attempt_is_reported_given_plain_stdout_then_keeps_log_framing_without_severity_color()
     {
        let line = "#thinking (1.234s) mid model=example-model phase=code context=12,345 B";

        let mut out = OutputCoordinator::new(RecordingSink::new());
        out.thinking_log(
            "mid",
            "example-model",
            "code",
            12_345,
            Duration::from_millis(1_234),
        )
        .unwrap();
        assert_eq!(out.sink().stdout(), format!("{line}\n"));
        assert_eq!(out.sink().stderr(), "", "the report is a stdout log");
        assert_eq!(out.last(), Some(OutputKind::Log));
        assert_eq!(
            out.sink().ops,
            [Op::Stdout(format!("{line}\n")), Op::FlushStdout,]
        );
        assert!(
            !out.sink().stdout().contains('\u{1b}'),
            "the plain report carries no ANSI"
        );

        let mut spaced = OutputCoordinator::new(RecordingSink::new());
        spaced.narration("# done").unwrap();
        spaced
            .thinking_log(
                "mid",
                "example-model",
                "code",
                12_345,
                Duration::from_millis(1_234),
            )
            .unwrap();
        assert_eq!(
            spaced.sink().stdout(),
            format!("# done\n\n{line}\n"),
            "the coordinator supplies its normal log separator and newline"
        );

        let mut terminal = OutputCoordinator::new(RecordingSink::new().stdout_terminal(true));
        terminal
            .thinking_log(
                "mid",
                "example-model",
                "code",
                12_345,
                Duration::from_millis(1_234),
            )
            .unwrap();
        assert_eq!(
            terminal.sink().stdout(),
            format!("{CYAN}{line}{STYLE_END}\n"),
            "a terminal report is whole-payload cyan, never severity-coloured"
        );
        assert!(
            !terminal.sink().stdout().contains(INFO_FG),
            "the report does not acquire the severity marker colour"
        );
    }

    /// Regression: every non-thinking operation keeps its established
    /// `[INFO] <label> elapsed: ...` line and stream, and the old model-attempt
    /// thinking elapsed line is gone.
    #[test]
    fn when_non_thinking_operation_finishes_given_elapsed_duration_then_keeps_existing_log_format()
    {
        let elapsed = Duration::from_millis(1_234);
        for label in [
            "bash",
            "execute_script",
            "plan-agent ask",
            "pipeline total",
            "ticket queue",
        ] {
            assert_eq!(
                elapsed_line(label, elapsed),
                format!("[INFO] {label} elapsed: 1.234 seconds")
            );
        }

        let mut out = OutputCoordinator::new(RecordingSink::new());
        out.elapsed_log("bash", elapsed).unwrap();
        assert_eq!(out.sink().stdout(), "[INFO] bash elapsed: 1.234 seconds\n");
        assert_eq!(out.last(), Some(OutputKind::Log));
        assert!(
            !out.sink().stdout().contains("#thinking"),
            "a non-thinking operation never writes a thinking report"
        );
        assert!(
            !out.sink().stdout().contains("thinking elapsed"),
            "the retired model-attempt elapsed line must not return"
        );
    }

    // -- Output coordinator -------------------------------------------------

    /// One byte-level operation a [`RecordingSink`] observed, in the order it
    /// happened: the writer side of the coordinator's policy.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Op {
        Stdout(String),
        FlushStdout,
        Stderr(String),
        FlushStderr,
    }

    /// A controlled sink: it records every write and flush in order, it can be
    /// switched to fail, and it answers the two terminal-capability queries
    /// from its own switches. Tests read it back through
    /// [`OutputCoordinator::sink`], so the coordinator's bytes, flush order and
    /// colouring are asserted without a terminal and without touching the
    /// process's real streams.
    ///
    /// Both switches default to `false`, the non-terminal answer, which is what
    /// keeps every plain-output assertion in this module valid: a test only
    /// sees escape codes when it asks for a terminal stream by name.
    #[derive(Debug, Default)]
    struct RecordingSink {
        fail: bool,
        fail_flush: bool,
        stdout_terminal: bool,
        stderr_terminal: bool,
        ops: Vec<Op>,
    }

    impl RecordingSink {
        fn new() -> Self {
            Self::default()
        }

        /// Treat stdout as a terminal, or as a redirected stream, without
        /// saying anything about stderr.
        fn stdout_terminal(mut self, terminal: bool) -> Self {
            self.stdout_terminal = terminal;
            self
        }

        /// Treat stderr as a terminal, or as a redirected stream, without
        /// saying anything about stdout.
        fn stderr_terminal(mut self, terminal: bool) -> Self {
            self.stderr_terminal = terminal;
            self
        }

        /// Everything written to stdout, concatenated.
        fn stdout(&self) -> String {
            self.ops
                .iter()
                .filter_map(|op| match op {
                    Op::Stdout(bytes) => Some(bytes.as_str()),
                    _ => None,
                })
                .collect()
        }

        /// Everything written to stderr, concatenated.
        fn stderr(&self) -> String {
            self.ops
                .iter()
                .filter_map(|op| match op {
                    Op::Stderr(bytes) => Some(bytes.as_str()),
                    _ => None,
                })
                .collect()
        }
    }

    impl OutputSink for RecordingSink {
        fn stdout_is_terminal(&self) -> bool {
            self.stdout_terminal
        }

        fn stderr_is_terminal(&self) -> bool {
            self.stderr_terminal
        }

        fn write_stdout(&mut self, bytes: &str) -> std::io::Result<()> {
            if self.fail {
                return Err(std::io::Error::other("stdout write failed"));
            }
            self.ops.push(Op::Stdout(bytes.to_string()));
            Ok(())
        }

        fn flush_stdout(&mut self) -> std::io::Result<()> {
            if self.fail_flush {
                return Err(std::io::Error::other("stdout flush failed"));
            }
            self.ops.push(Op::FlushStdout);
            Ok(())
        }

        fn write_stderr(&mut self, bytes: &str) -> std::io::Result<()> {
            if self.fail {
                return Err(std::io::Error::other("stderr write failed"));
            }
            self.ops.push(Op::Stderr(bytes.to_string()));
            Ok(())
        }

        fn flush_stderr(&mut self) -> std::io::Result<()> {
            self.ops.push(Op::FlushStderr);
            Ok(())
        }
    }

    /// A fresh coordinator remembers no kind, and the first message of every
    /// stdout kind is written as the payload plus one newline, with no
    /// separator in front of it.
    #[test]
    fn the_first_message_of_every_kind_gets_no_separator() {
        let mut calls = OutputCoordinator::new(RecordingSink::new());
        calls.tool_call("$ ls -la").unwrap();
        assert_eq!(calls.sink().stdout(), "$ ls -la\n");
        assert_eq!(calls.last(), Some(OutputKind::ToolCall));

        let mut narration = OutputCoordinator::new(RecordingSink::new());
        narration.narration("# hello").unwrap();
        assert_eq!(narration.sink().stdout(), "# hello\n");
        assert_eq!(narration.last(), Some(OutputKind::Narration));

        let mut info = OutputCoordinator::new(RecordingSink::new());
        info.stdout_log("[INFO] ready").unwrap();
        assert_eq!(info.sink().stdout(), "[INFO] ready\n");
        assert_eq!(info.sink().stderr(), "", "a stdout log is not framed");
        assert_eq!(info.last(), Some(OutputKind::Log));
    }

    /// Edge-case minimum/first-event boundary: a fresh coordinator writes its
    /// first stderr severity log with no synthetic leading separator and one
    /// terminating newline, and an ordinary single stdout severity log does the
    /// same on its own stream.
    #[test]
    fn when_logs_start_given_fresh_coordinator_then_write_without_a_leading_separator() {
        let mut stderr_first = OutputCoordinator::new(RecordingSink::new());
        stderr_first.stderr_log("[ERROR] boom").unwrap();
        assert_eq!(stderr_first.sink().stdout(), "");
        assert_eq!(stderr_first.sink().stderr(), "[ERROR] boom\n");
        assert_eq!(
            stderr_first.sink().ops,
            [
                Op::FlushStdout,
                Op::Stderr("[ERROR] boom\n".into()),
                Op::FlushStderr,
            ],
            "the first event is not preceded by a synthetic blank line"
        );
        assert_eq!(stderr_first.last(), Some(OutputKind::Log));

        let mut stdout_first = OutputCoordinator::new(RecordingSink::new());
        stdout_first.stdout_log("[INFO] ready").unwrap();
        assert_eq!(stdout_first.sink().stdout(), "[INFO] ready\n");
        assert_eq!(stdout_first.sink().stderr(), "");
        assert_eq!(stdout_first.last(), Some(OutputKind::Log));
    }

    /// All nine ordered kind pairs: exactly one blank line between different
    /// kinds and none between the same kind, in both directions. The `A`/`B`
    /// payloads make the exact bytes -- separator included -- visible.
    #[test]
    fn every_directed_kind_change_adds_exactly_one_blank_line() {
        let kinds = [OutputKind::ToolCall, OutputKind::Narration, OutputKind::Log];
        for from in kinds {
            for to in kinds {
                let mut out = OutputCoordinator::new(RecordingSink::new());
                out.stdout(from, "A").unwrap();
                out.stdout(to, "B").unwrap();
                let expected = if from == to { "A\nB\n" } else { "A\n\nB\n" };
                assert_eq!(out.sink().stdout(), expected, "{from:?} then {to:?}");
                assert_eq!(out.last(), Some(to));
                assert_eq!(out.sink().stderr(), "", "{from:?} then {to:?}");
            }
        }
    }

    /// Same-kind sequences for all three kinds: consecutive call lines and
    /// consecutive narrations read as one block, and consecutive stdout logs
    /// stay adjacent whatever their severity -- severity is not a kind.
    #[test]
    fn same_kind_stdout_sequences_get_no_separator() {
        let mut calls = OutputCoordinator::new(RecordingSink::new());
        calls.tool_call("$ one").unwrap();
        calls.tool_call("$ two").unwrap();
        assert_eq!(calls.sink().stdout(), "$ one\n$ two\n");

        let mut narration = OutputCoordinator::new(RecordingSink::new());
        narration.narration("# one").unwrap();
        narration.narration("# two").unwrap();
        assert_eq!(narration.sink().stdout(), "# one\n# two\n");

        let mut logs = OutputCoordinator::new(RecordingSink::new());
        logs.stdout_log("[INFO] one").unwrap();
        logs.stdout_log("[WARN] two").unwrap();
        logs.stdout_log("[ERROR] three").unwrap();
        assert_eq!(
            logs.sink().stdout(),
            "[INFO] one\n[WARN] two\n[ERROR] three\n"
        );
    }

    /// Two consecutive stderr severity logs are one log block: each owns one
    /// terminating newline, with no empty line and no merged line between them.
    #[test]
    fn when_stderr_logs_repeat_given_severity_messages_then_write_adjacent_lines() {
        let mut out = OutputCoordinator::new(RecordingSink::new());
        out.stderr_log("[WARN] one").unwrap();
        out.stderr_log("[ERROR] two").unwrap();

        assert_eq!(out.sink().stderr(), "[WARN] one\n[ERROR] two\n");
        assert!(
            !out.sink().stderr().contains("\n\n"),
            "adjacent log events have no blank-line separator"
        );
        assert_eq!(
            out.sink().ops,
            [
                Op::FlushStdout,
                Op::Stderr("[WARN] one\n".into()),
                Op::FlushStderr,
                Op::FlushStdout,
                Op::Stderr("[ERROR] two\n".into()),
                Op::FlushStderr,
            ],
            "each diagnostic is written and flushed once, in order"
        );
        assert_eq!(out.last(), Some(OutputKind::Log));
    }

    /// Malformed/invalid payload boundary: a caller's newlines and
    /// unrecognized non-severity text are never normalized, on either stream.
    #[test]
    fn when_caller_supplies_newlines_given_non_severity_payload_then_preserve_explicit_formatting()
    {
        let banner = "=== banner ===\n\nline one\nline two";
        let mut out = OutputCoordinator::new(RecordingSink::new());
        out.stdout_log(banner).unwrap();
        assert_eq!(out.sink().stdout(), format!("{banner}\n"));

        let mut diagnostic = OutputCoordinator::new(RecordingSink::new());
        let message = "[TRACE] first line\nsecond line\n\nfourth line";
        diagnostic.stderr_log(message).unwrap();
        assert_eq!(diagnostic.sink().stderr(), format!("{message}\n"));

        let mut terminated = OutputCoordinator::new(RecordingSink::new());
        terminated
            .stderr_log("[TRACE] already\nterminated\n")
            .unwrap();
        assert_eq!(
            terminated.sink().stderr(),
            "[TRACE] already\nterminated\n\n",
            "the caller's trailing newline and the event terminator both survive"
        );
    }

    /// A stderr diagnostic counts as a log for the stdout kind rule: narration
    /// after a diagnostic gets exactly one separator, while a stdout log after
    /// a stderr log is the same kind and gets none.
    #[test]
    fn when_logs_are_written_given_stdout_and_stderr_then_share_one_log_kind() {
        let mut out = OutputCoordinator::new(RecordingSink::new());
        out.stderr_log("[WARN] a").unwrap();
        out.narration("# hi").unwrap();
        assert_eq!(out.sink().stderr(), "[WARN] a\n");
        assert_eq!(out.sink().stdout(), "\n# hi\n");

        let mut out = OutputCoordinator::new(RecordingSink::new());
        out.stdout_log("[INFO] a").unwrap();
        out.stderr_log("[WARN] b").unwrap();
        out.stdout_log("[INFO] c").unwrap();
        assert_eq!(out.sink().stdout(), "[INFO] a\n[INFO] c\n");
        assert_eq!(out.sink().stderr(), "[WARN] b\n");
    }

    /// Core entry-point behavior: a fresh coordinator keeps mixed-severity log
    /// events adjacent across streams, in global event order, and flushes the
    /// previous stream before the next one writes.
    #[test]
    fn when_logs_are_written_given_events_across_streams_then_preserve_adjacency_and_order() {
        let mut out = OutputCoordinator::new(RecordingSink::new());
        out.stdout_log("[INFO] one").unwrap();
        out.stderr_log("[WARN] two").unwrap();
        out.stdout_log("[ERROR] three").unwrap();

        assert_eq!(out.sink().stdout(), "[INFO] one\n[ERROR] three\n");
        assert_eq!(out.sink().stderr(), "[WARN] two\n");
        assert_eq!(
            out.sink().ops,
            [
                Op::Stdout("[INFO] one\n".into()),
                Op::FlushStdout,
                Op::FlushStdout,
                Op::Stderr("[WARN] two\n".into()),
                Op::FlushStderr,
                Op::FlushStderr,
                Op::Stdout("[ERROR] three\n".into()),
                Op::FlushStdout,
            ],
            "stdout lands before stderr, and stderr is flushed before the final stdout log"
        );
        assert_eq!(out.last(), Some(OutputKind::Log));
    }

    /// Maximum/upper boundary: a long log payload is neither truncated nor
    /// merged with the event around it, on either stream.
    #[test]
    fn when_logs_reach_long_payloads_given_consecutive_events_then_preserve_complete_lines() {
        let first = format!("[WARN] {}", "a".repeat(20_000));
        let second = format!("[ERROR] {}", "b".repeat(20_000));

        let mut out = OutputCoordinator::new(RecordingSink::new());
        out.stdout_log("[INFO] before").unwrap();
        out.stderr_log(&first).unwrap();
        out.stderr_log(&second).unwrap();
        out.stdout_log("[ERROR] after").unwrap();

        assert_eq!(
            out.sink().stderr(),
            format!("{first}\n{second}\n"),
            "two long stderr events keep every payload byte and both terminators"
        );
        assert_eq!(
            out.sink().stdout(),
            "[INFO] before\n[ERROR] after\n",
            "the long stderr block does not disturb stdout log adjacency"
        );
        assert_eq!(out.sink().stderr().len(), first.len() + second.len() + 2);
        assert_eq!(out.sink().stderr().lines().count(), 2);
        assert!(!out.sink().stderr().contains("..."));
        assert_eq!(out.last(), Some(OutputKind::Log));
    }

    /// Contract: log/non-log transitions get exactly one blank line in either
    /// direction, on the same stream and across streams.
    #[test]
    fn when_logs_and_non_log_output_alternate_given_either_order_then_keep_exactly_one_blank_line()
    {
        // Same stream: narration to stdout log and stdout log back to narration.
        let mut same = OutputCoordinator::new(RecordingSink::new());
        same.narration("# before").unwrap();
        same.stdout_log("[INFO] middle").unwrap();
        same.narration("# after").unwrap();
        assert_eq!(
            same.sink().stdout(),
            "# before\n\n[INFO] middle\n\n# after\n"
        );

        // Cross streams: stdout narration to stderr log.
        let mut out_to_err = OutputCoordinator::new(RecordingSink::new());
        out_to_err.narration("# before").unwrap();
        out_to_err.stderr_log("[WARN] down").unwrap();
        assert_eq!(out_to_err.sink().stdout(), "# before\n");
        assert_eq!(
            out_to_err.sink().stderr(),
            "\n[WARN] down\n",
            "the separator precedes the first stderr event"
        );

        // Cross streams: stderr log to stdout narration.
        let mut err_to_out = OutputCoordinator::new(RecordingSink::new());
        err_to_out.stderr_log("[WARN] down").unwrap();
        err_to_out.narration("# after").unwrap();
        assert_eq!(err_to_out.sink().stderr(), "[WARN] down\n");
        assert_eq!(
            err_to_out.sink().stdout(),
            "\n# after\n",
            "the separator precedes the first stdout event"
        );

        // Cross streams with a tool call in each direction.
        let mut call_to_err = OutputCoordinator::new(RecordingSink::new());
        call_to_err.tool_call("$ ls").unwrap();
        call_to_err.stderr_log("[WARN] down").unwrap();
        assert_eq!(call_to_err.sink().stderr(), "\n[WARN] down\n");

        let mut err_to_call = OutputCoordinator::new(RecordingSink::new());
        err_to_call.stderr_log("[WARN] down").unwrap();
        err_to_call.tool_call("$ ls").unwrap();
        assert_eq!(err_to_call.sink().stdout(), "\n$ ls\n");
    }

    /// A stdout payload is written verbatim: its own newlines are neither
    /// trimmed nor normalized, so a banner that carries blank lines of its own
    /// keeps them and the coordinator only adds its separator and terminator.
    #[test]
    fn stdout_payloads_are_not_normalized() {
        let mut out = OutputCoordinator::new(RecordingSink::new());
        out.stdout_log("\n=== Spec agent ===\n").unwrap();
        out.narration("# hi\n").unwrap();
        assert_eq!(out.sink().stdout(), "\n=== Spec agent ===\n\n\n# hi\n\n");
    }

    /// The remembered kind is run-wide: successive operations observe what the
    /// earlier ones printed, so a message is spaced against the last output of
    /// any component, not against the start of this one.
    #[test]
    fn the_remembered_kind_persists_across_successive_operations() {
        let mut out = OutputCoordinator::new(RecordingSink::new());

        // A first component: one call line, then two logs of different
        // severities.
        out.tool_call("$ cargo test").unwrap();
        out.stdout_log("[INFO] plan written").unwrap();
        out.stdout_log("[ERROR] retry budget low").unwrap();

        // A second component continues where the first stopped: the log kind
        // is still remembered, so the logs above stayed adjacent and this
        // narration is spaced against a log.
        out.narration("# resuming").unwrap();

        assert_eq!(
            out.sink().stdout(),
            "$ cargo test\n\n[INFO] plan written\n[ERROR] retry budget low\n\n# resuming\n"
        );
        assert_eq!(out.last(), Some(OutputKind::Narration));
    }

    /// Two coordinators do not share state: a test can hold its own isolated
    /// transcript, so parallel unit tests never race on one process-global
    /// remembered kind.
    #[test]
    fn separate_coordinators_have_isolated_state() {
        let mut first = OutputCoordinator::new(RecordingSink::new());
        first.narration("# one").unwrap();

        let mut second = OutputCoordinator::new(RecordingSink::new());
        assert_eq!(second.last(), None);
        second.tool_call("$ two").unwrap();
        assert_eq!(second.sink().stdout(), "$ two\n");
    }

    /// Flush ordering around stream changes, asserted as the exact operation
    /// sequence: stdout is flushed before a stderr diagnostic, and stderr is
    /// flushed before output returns to stdout.
    #[test]
    fn when_logs_change_streams_given_non_log_output_then_keep_flush_order_and_spacing() {
        let mut out = OutputCoordinator::new(RecordingSink::new());
        out.narration("# hi").unwrap();
        out.stderr_log("[WARN] x").unwrap();
        assert_eq!(
            out.sink().ops,
            [
                Op::Stdout("# hi\n".into()),
                Op::FlushStdout,
                Op::FlushStdout,
                Op::Stderr("\n[WARN] x\n".into()),
                Op::FlushStderr,
            ]
        );

        let mut out = OutputCoordinator::new(RecordingSink::new());
        out.stderr_log("[WARN] x").unwrap();
        out.narration("# hi").unwrap();
        assert_eq!(
            out.sink().ops,
            [
                Op::FlushStdout,
                Op::Stderr("[WARN] x\n".into()),
                Op::FlushStderr,
                Op::FlushStderr,
                Op::Stdout("\n# hi\n".into()),
                Op::FlushStdout,
            ]
        );
    }

    /// Narration eligibility: a missing or whitespace-only block has nothing
    /// to narrate and is skipped, exactly as the agent loop skipped it before
    /// the coordinator existed.
    #[test]
    fn printable_narration_skips_missing_and_whitespace_only_blocks() {
        assert_eq!(printable_narration(None), None);
        assert_eq!(printable_narration(Some("")), None);
        assert_eq!(printable_narration(Some("   \n\t\n")), None);
        assert_eq!(printable_narration(Some("# a line")), Some("# a line"));
    }

    /// A skipped message never reaches the coordinator: it writes nothing and
    /// the remembered kind is still the last message that did reach it, so the
    /// next message is spaced against that one.
    #[test]
    fn skipped_narration_writes_nothing_and_preserves_the_remembered_kind() {
        let mut out = OutputCoordinator::new(RecordingSink::new());
        out.narration("# first").unwrap();
        let written = out.sink().ops.len();

        for content in [None, Some(""), Some("  \t\n ")] {
            if let Some(text) = printable_narration(content) {
                out.narration(text).unwrap();
            }
        }

        assert_eq!(out.sink().ops.len(), written, "a skip must not write");
        assert_eq!(out.last(), Some(OutputKind::Narration));

        out.tool_call("$ next").unwrap();
        assert_eq!(out.sink().stdout(), "# first\n\n$ next\n");
    }

    /// Contract: the remembered kind and spacing are committed only after the
    /// write and flush succeed; a pending event that fails leaves the previous
    /// successful event in place.
    #[test]
    fn when_output_fails_given_pending_events_then_remember_only_successful_writes() {
        let mut out = OutputCoordinator::new(RecordingSink::new());
        out.narration("# ok").unwrap();
        assert_eq!(out.last(), Some(OutputKind::Narration));

        out.sink_mut().fail = true;
        assert!(out.tool_call("$ boom").is_err());
        assert_eq!(out.last(), Some(OutputKind::Narration));
        assert_eq!(
            out.sink().ops,
            [Op::Stdout("# ok\n".into()), Op::FlushStdout],
            "a failed write must not reach the sink"
        );

        assert!(out.stderr_log("[ERROR] boom").is_err());
        assert_eq!(out.sink().stderr(), "", "a failed event writes no bytes");
        assert_eq!(
            out.last(),
            Some(OutputKind::Narration),
            "a failed stderr event commits no log kind"
        );

        out.sink_mut().fail = false;
        out.sink_mut().fail_flush = true;
        assert!(out.stdout_log("[INFO] pending").is_err());
        assert_eq!(
            out.last(),
            Some(OutputKind::Narration),
            "a failed flush commits no log kind"
        );
    }

    /// Production exposes exactly one coordinator, over the real streams, and
    /// its sink is the stdio one that actually carries the bytes.
    #[test]
    fn production_exposes_one_shared_stdio_coordinator() {
        fn assert_stdio(_: &std::sync::MutexGuard<'static, OutputCoordinator<StdioSink>>) {}

        let guard = shared();
        assert_stdio(&guard);
        // Other tests -- the agent loop's, for one -- now route their own
        // transcript through this coordinator, so whichever kind is remembered
        // here is some other test's write, not this one's. What this test pins
        // is the coordinator itself: exactly one exists, over the real stdio
        // streams, and it is borrowable from anywhere in the process.
        let _ = guard.last();
    }

    // -- Tool-call colouring at the coordinator boundary --------------------

    /// A redirected stdout gets the exact plain bytes through the coordinator:
    /// the `$ <target>` line, one newline, no escape sequence anywhere.
    #[test]
    fn the_coordinator_writes_a_plain_tool_call_when_stdout_is_not_a_terminal() {
        let mut out = OutputCoordinator::new(RecordingSink::new());
        out.tool_call("$ echo ejaj-ok").unwrap();

        assert_eq!(out.sink().stdout(), "$ echo ejaj-ok\n");
        assert!(
            !out.sink().stdout().contains('\u{1b}'),
            "a redirected transcript carries no ANSI"
        );
        assert_eq!(
            out.sink().ops,
            [Op::Stdout("$ echo ejaj-ok\n".into()), Op::FlushStdout]
        );
        assert_eq!(out.last(), Some(OutputKind::ToolCall));
    }

    /// The recording sink is non-terminal on both streams until a test says
    /// otherwise, and the two switches are answered independently.
    #[test]
    fn the_recording_sink_defaults_to_non_terminal_streams() {
        let sink = RecordingSink::new();
        assert!(!sink.stdout_is_terminal());
        assert!(!sink.stderr_is_terminal());

        let stdout_only = RecordingSink::new().stdout_terminal(true);
        assert!(stdout_only.stdout_is_terminal());
        assert!(
            !stdout_only.stderr_is_terminal(),
            "stderr is not inherited from stdout"
        );

        let stderr_only = RecordingSink::new().stderr_terminal(true);
        assert!(stderr_only.stderr_is_terminal());
        assert!(
            !stderr_only.stdout_is_terminal(),
            "stdout is not inherited from stderr"
        );
    }

    /// The terminal rendering reaches the stream through the coordinator: the
    /// whole call line in cyan, the full reset, then the coordinator's single
    /// newline, and nothing at all on stderr.
    #[test]
    fn the_coordinator_colours_a_tool_call_on_a_terminal_stdout() {
        let mut out = OutputCoordinator::new(RecordingSink::new().stdout_terminal(true));
        out.tool_call("$ ls -la").unwrap();

        assert_eq!(out.sink().stdout(), format!("{CYAN}$ ls -la{STYLE_END}\n"));
        assert_eq!(out.sink().stderr(), "", "a call line is stdout output");
        assert_eq!(out.last(), Some(OutputKind::ToolCall));
    }

    /// The colour lives inside the payload, so the framing around it is
    /// untouched: the separator in front of a coloured call line and the
    /// newline after it stay plain, and the flush order is the established one.
    #[test]
    fn a_coloured_tool_call_keeps_the_separator_newline_and_flush() {
        let mut out = OutputCoordinator::new(RecordingSink::new().stdout_terminal(true));
        out.narration("# hi").unwrap();
        out.tool_call("$ cargo test").unwrap();

        assert_eq!(
            out.sink().stdout(),
            format!("# hi\n\n{CYAN}$ cargo test{STYLE_END}\n")
        );
        assert_eq!(
            out.sink().ops,
            [
                Op::Stdout("# hi\n".into()),
                Op::FlushStdout,
                Op::Stdout(format!("\n{CYAN}$ cargo test{STYLE_END}\n")),
                Op::FlushStdout,
            ]
        );

        // Two call lines in a row are still one kind: the second pays no
        // separator, and each of them carries its own colour and reset.
        out.tool_call("$ ls").unwrap();
        assert_eq!(
            out.sink().stdout(),
            format!("# hi\n\n{CYAN}$ cargo test{STYLE_END}\n{CYAN}$ ls{STYLE_END}\n")
        );
        assert_eq!(out.last(), Some(OutputKind::ToolCall));
    }

    /// Narration is written exactly as its caller rendered it, and an
    /// unrecognized log is not coloured either: only a recognized severity tag
    /// takes a colour, and never the tool-call cyan.
    #[test]
    fn narration_and_untagged_logs_stay_unstyled_on_a_terminal_stdout() {
        let mut out = OutputCoordinator::new(RecordingSink::new().stdout_terminal(true));
        out.narration(&render_agent_text("# hi", true)).unwrap();
        out.stdout_log("[DEBUG] verbose").unwrap();

        let stdout = out.sink().stdout();
        assert_eq!(
            stdout,
            format!("{HEADING}{BOLD}{UNDERLINE}hi{STYLE_END}\n\n[DEBUG] verbose\n")
        );
        assert!(
            !stdout.contains(CYAN),
            "no narration or log line is painted in the call colour: {stdout:?}"
        );
    }

    /// stdout's terminal answer comes from the sink and never from stderr's: a
    /// terminal stdout colours a call line even with stderr redirected, and a
    /// redirected stdout writes plain bytes even with stderr on a terminal,
    /// while each stream's own logs follow that stream's answer.
    #[test]
    fn when_stderr_is_redirected_given_terminal_capabilities_then_preserve_stream_specific_rendering()
     {
        let mut terminal_stdout = OutputCoordinator::new(
            RecordingSink::new()
                .stdout_terminal(true)
                .stderr_terminal(false),
        );
        terminal_stdout.tool_call("$ ls").unwrap();
        terminal_stdout.stderr_log("[WARN] x").unwrap();
        assert_eq!(
            terminal_stdout.sink().stdout(),
            format!("{CYAN}$ ls{STYLE_END}\n")
        );
        assert_eq!(
            terminal_stdout.sink().stderr(),
            "\n[WARN] x\n",
            "a redirected stderr keeps the plain diagnostic bytes and its transition separator"
        );

        let mut terminal_stderr = OutputCoordinator::new(
            RecordingSink::new()
                .stdout_terminal(false)
                .stderr_terminal(true),
        );
        terminal_stderr.tool_call("$ ls").unwrap();
        terminal_stderr.stderr_log("[WARN] x").unwrap();
        assert_eq!(
            terminal_stderr.sink().stdout(),
            "$ ls\n",
            "a redirected stdout keeps the plain bytes"
        );
        assert!(
            !terminal_stderr.sink().stdout().contains('\u{1b}'),
            "and no escape sequence"
        );
        assert_eq!(
            terminal_stderr.sink().stderr(),
            format!("\n{WARN_FG}[WARN]{STYLE_END} x\n")
        );
        assert!(
            !terminal_stderr.sink().stderr().contains(CYAN),
            "the diagnostic takes the severity colour, never the call cyan"
        );
    }

    /// The colour hangs off the tool-call kind rather than off one entry point:
    /// a call written through the generic `stdout` renders exactly like one
    /// written through `tool_call`, and another kind with the same bytes does
    /// not.
    #[test]
    fn the_tool_call_kind_is_coloured_through_either_entry_point() {
        let mut named = OutputCoordinator::new(RecordingSink::new().stdout_terminal(true));
        named.tool_call("$ ls").unwrap();

        let mut generic = OutputCoordinator::new(RecordingSink::new().stdout_terminal(true));
        generic.stdout(OutputKind::ToolCall, "$ ls").unwrap();

        assert_eq!(named.sink().stdout(), generic.sink().stdout());
        assert_eq!(generic.sink().stdout(), format!("{CYAN}$ ls{STYLE_END}\n"));

        let mut log = OutputCoordinator::new(RecordingSink::new().stdout_terminal(true));
        log.stdout(OutputKind::Log, "$ ls").unwrap();
        assert_eq!(log.sink().stdout(), "$ ls\n", "a log is not a call line");
    }

    /// Production answers both capability queries from the real streams, each
    /// one on its own, rather than from a constant.
    #[test]
    fn the_stdio_sink_asks_each_real_stream() {
        let sink = StdioSink;
        assert_eq!(sink.stdout_is_terminal(), io::stdout().is_terminal());
        assert_eq!(sink.stderr_is_terminal(), io::stderr().is_terminal());
    }

    // -- Log colouring at the coordinator boundary --------------------------

    /// A stdout log is rendered against stdout's own terminal answer: on a
    /// terminal the recognized tag is coloured, and the separator and newline
    /// stay plain.
    #[test]
    fn the_coordinator_colours_a_stdout_log_tag_on_a_terminal_stdout() {
        let mut out = OutputCoordinator::new(RecordingSink::new().stdout_terminal(true));
        out.stdout_log("[INFO] ready").unwrap();

        assert_eq!(
            out.sink().stdout(),
            format!("{INFO_FG}[INFO]{STYLE_END} ready\n")
        );
        assert_eq!(out.sink().stderr(), "", "a stdout log is not framed");
        assert_eq!(
            out.sink().ops,
            [
                Op::Stdout(format!("{INFO_FG}[INFO]{STYLE_END} ready\n")),
                Op::FlushStdout,
            ]
        );
        assert_eq!(out.last(), Some(OutputKind::Log));
    }

    /// A stderr diagnostic is rendered against stderr's own terminal answer,
    /// while its event terminator stays plain.
    #[test]
    fn when_a_diagnostic_is_written_given_a_terminal_stream_then_colours_only_the_severity_tag() {
        let mut out = OutputCoordinator::new(RecordingSink::new().stderr_terminal(true));
        out.stderr_log("[WARN] slow").unwrap();

        assert_eq!(
            out.sink().stderr(),
            format!("{WARN_FG}[WARN]{STYLE_END} slow\n")
        );
        assert_eq!(out.sink().stdout(), "");
        assert_eq!(out.last(), Some(OutputKind::Log));
    }

    /// The two streams are rendered independently: a terminal stdout colours a
    /// stdout log while a redirected stderr leaves the diagnostic plain, and a
    /// redirected stdout leaves a stdout log plain while a terminal stderr
    /// colours the diagnostic.
    #[test]
    fn when_logs_are_written_given_independent_terminal_capabilities_then_render_each_stream_by_its_own_answer()
     {
        let mut terminal_stdout = OutputCoordinator::new(
            RecordingSink::new()
                .stdout_terminal(true)
                .stderr_terminal(false),
        );
        terminal_stdout.stdout_log("[INFO] on screen").unwrap();
        terminal_stdout.stderr_log("[ERROR] to a file").unwrap();
        assert_eq!(
            terminal_stdout.sink().stdout(),
            format!("{INFO_FG}[INFO]{STYLE_END} on screen\n")
        );
        assert_eq!(
            terminal_stdout.sink().stderr(),
            "[ERROR] to a file\n",
            "a redirected stderr keeps the plain diagnostic bytes"
        );
        assert!(
            !terminal_stdout.sink().stderr().contains('\u{1b}'),
            "a redirected stderr carries no ANSI"
        );

        let mut terminal_stderr = OutputCoordinator::new(
            RecordingSink::new()
                .stdout_terminal(false)
                .stderr_terminal(true),
        );
        terminal_stderr.stdout_log("[INFO] to a file").unwrap();
        terminal_stderr.stderr_log("[ERROR] on screen").unwrap();
        assert_eq!(
            terminal_stderr.sink().stdout(),
            "[INFO] to a file\n",
            "a redirected stdout keeps the plain bytes"
        );
        assert!(
            !terminal_stderr.sink().stdout().contains('\u{1b}'),
            "a redirected stdout carries no ANSI"
        );
        assert_eq!(
            terminal_stderr.sink().stderr(),
            format!("{ERROR_FG}[ERROR]{STYLE_END} on screen\n")
        );
    }

    /// A log the renderer does not recognize is written byte for byte even when
    /// its stream is a terminal: severity is the only thing that gets colour.
    #[test]
    fn when_diagnostics_are_rendered_given_severity_and_unrecognized_tags_then_preserve_color_rules()
     {
        let mut out = OutputCoordinator::new(
            RecordingSink::new()
                .stdout_terminal(true)
                .stderr_terminal(true),
        );
        out.stdout_log("[DEBUG] stdout verbose").unwrap();
        out.stderr_log("[TRACE] stderr verbose").unwrap();

        assert_eq!(out.sink().stdout(), "[DEBUG] stdout verbose\n");
        assert_eq!(out.sink().stderr(), "[TRACE] stderr verbose\n");
        assert!(!out.sink().stdout().contains('\u{1b}'));
        assert!(!out.sink().stderr().contains('\u{1b}'));
    }

    /// The framing around a coloured log is untouched: the stdout separator in
    /// front of a tagged log and the newline after it stay plain, two tagged
    /// logs of the same kind stay adjacent, and the flush order is unchanged.
    #[test]
    fn a_coloured_log_keeps_the_separator_newline_and_flush_order() {
        let mut out = OutputCoordinator::new(RecordingSink::new().stdout_terminal(true));
        out.narration("# hi").unwrap();
        out.stdout_log("[INFO] ready").unwrap();

        assert_eq!(
            out.sink().stdout(),
            format!("# hi\n\n{INFO_FG}[INFO]{STYLE_END} ready\n")
        );
        assert_eq!(
            out.sink().ops,
            [
                Op::Stdout("# hi\n".into()),
                Op::FlushStdout,
                Op::Stdout(format!("\n{INFO_FG}[INFO]{STYLE_END} ready\n")),
                Op::FlushStdout,
            ]
        );

        // Severity is not a kind: a second tagged log stays adjacent.
        out.stdout_log("[ERROR] down").unwrap();
        assert_eq!(
            out.sink().stdout(),
            format!(
                "# hi\n\n{INFO_FG}[INFO]{STYLE_END} ready\n\
                 {ERROR_FG}[ERROR]{STYLE_END} down\n"
            )
        );
        assert_eq!(out.last(), Some(OutputKind::Log));
    }

    /// A coloured stderr diagnostic keeps its single terminator, and the
    /// flush ordering around a stream change is unchanged.
    #[test]
    fn when_diagnostics_follow_logs_given_terminal_streams_then_keep_plain_payload_and_terminator()
    {
        let mut out = OutputCoordinator::new(RecordingSink::new().stderr_terminal(true));
        out.stderr_log("[ERROR] boom").unwrap();
        out.narration("# hi").unwrap();

        assert_eq!(
            out.sink().stderr(),
            format!("{ERROR_FG}[ERROR]{STYLE_END} boom\n")
        );
        assert_eq!(
            out.sink().ops,
            [
                Op::FlushStdout,
                Op::Stderr(format!("{ERROR_FG}[ERROR]{STYLE_END} boom\n")),
                Op::FlushStderr,
                Op::FlushStderr,
                Op::Stdout("\n# hi\n".into()),
                Op::FlushStdout,
            ]
        );
        assert_eq!(out.last(), Some(OutputKind::Narration));
    }

    /// The remembered kind still changes only after a successful write, with a
    /// coloured log as the attempted message.
    #[test]
    fn a_failed_coloured_log_leaves_the_remembered_kind_alone() {
        let mut out = OutputCoordinator::new(RecordingSink::new().stdout_terminal(true));
        out.narration("# ok").unwrap();
        assert_eq!(out.last(), Some(OutputKind::Narration));

        out.sink_mut().fail = true;
        assert!(out.stdout_log("[ERROR] boom").is_err());
        assert_eq!(out.last(), Some(OutputKind::Narration));
        assert_eq!(
            out.sink().ops,
            [Op::Stdout("# ok\n".into()), Op::FlushStdout],
            "a failed write must not reach the sink"
        );

        assert!(out.stderr_log("[ERROR] boom").is_err());
        assert_eq!(out.last(), Some(OutputKind::Narration));
    }

    // -- Tool-execution timing ----------------------------------------------

    /// Zero/empty case: the smallest operation there is -- no measurable time,
    /// an empty result -- is still reported once, and zero, which is what every
    /// sub-millisecond duration floors to, is rendered with a full three-digit
    /// fraction.
    #[test]
    fn when_zero_duration_is_reported_given_empty_result_then_formats_zero_seconds() {
        let mut out = OutputCoordinator::new(RecordingSink::new());

        let empty = crate::timing::measure(&mut out, "bash", String::new);
        assert_eq!(empty, "", "the empty result is handed back unchanged");

        out.elapsed_log("bash", Duration::ZERO).unwrap();

        assert_eq!(
            out.sink().stdout().lines().last(),
            Some("[INFO] bash elapsed: 0.000 seconds"),
            "a zero duration reads as zero-point-zero-zero-zero"
        );
        assert_eq!(
            out.sink().stdout().lines().count(),
            2,
            "one report per operation, exactly"
        );
        assert_eq!(
            elapsed_line("bash", Duration::ZERO),
            "[INFO] bash elapsed: 0.000 seconds"
        );
    }

    /// Minimum valid boundary: one millisecond is one thousandth of a second,
    /// written with a zero-padded three-digit fraction, for both labels -- and
    /// the coordinator writes exactly that line and nothing else.
    #[test]
    fn when_duration_is_reported_given_one_millisecond_then_formats_three_fractional_digits() {
        let elapsed = Duration::from_millis(1);

        assert_eq!(
            elapsed_line("bash", elapsed),
            "[INFO] bash elapsed: 0.001 seconds"
        );
        assert_eq!(
            elapsed_line("execute_script", elapsed),
            "[INFO] execute_script elapsed: 0.001 seconds"
        );

        let mut out = OutputCoordinator::new(RecordingSink::new());
        out.elapsed_log("execute_script", elapsed).unwrap();
        assert_eq!(
            out.sink().stdout(),
            "[INFO] execute_script elapsed: 0.001 seconds\n"
        );
    }

    /// Every duration below a millisecond floors to `0.000`: the fraction is
    /// truncated rather than rounded up, and the first whole millisecond is
    /// where the figure moves.
    #[test]
    fn when_duration_is_reported_given_submillisecond_elapsed_time_then_floors_to_zero() {
        for elapsed in [
            Duration::ZERO,
            Duration::from_nanos(1),
            Duration::from_micros(999),
            Duration::from_nanos(999_999),
        ] {
            assert_eq!(
                elapsed_line("bash", elapsed),
                "[INFO] bash elapsed: 0.000 seconds",
                "{elapsed:?} must floor to zero"
            );
        }

        assert_eq!(
            elapsed_line("bash", Duration::from_micros(1_000)),
            "[INFO] bash elapsed: 0.001 seconds",
            "the first whole millisecond must not be floored away"
        );
    }

    /// Upper boundary: a duration past a minute is nothing special. The seconds
    /// keep counting, the fraction keeps its three digits, and no unit ever
    /// changes on either side of a minute or an hour.
    #[test]
    fn when_duration_exceeds_one_minute_given_fixed_milliseconds_then_preserves_seconds() {
        assert_eq!(
            elapsed_line("bash", Duration::from_millis(61_500)),
            "[INFO] bash elapsed: 61.500 seconds"
        );
        assert_eq!(
            elapsed_line("execute_script", Duration::from_secs(3_600)),
            "[INFO] execute_script elapsed: 3600.000 seconds"
        );

        assert_eq!(
            elapsed_line(
                "bash",
                Duration::from_secs(12_345) + Duration::from_millis(678)
            ),
            "[INFO] bash elapsed: 12345.678 seconds",
            "a duration past three hours keeps every whole second, uncapped"
        );

        let mut out = OutputCoordinator::new(RecordingSink::new());
        out.elapsed_log("bash", Duration::from_millis(61_500))
            .unwrap();
        assert_eq!(out.sink().stdout(), "[INFO] bash elapsed: 61.500 seconds\n");
        assert_eq!(out.last(), Some(OutputKind::Log));

        // The same long duration through the real reporting path: the exact
        // seconds and fraction reach the coordinator's stdout, nothing rewritten.
        out.elapsed_log(
            "bash",
            Duration::from_secs(12_345) + Duration::from_millis(678),
        )
        .unwrap();
        assert_eq!(
            out.sink().stdout(),
            "[INFO] bash elapsed: 61.500 seconds\n[INFO] bash elapsed: 12345.678 seconds\n"
        );
    }

    /// Contract: the redirected transcript is the exact plain line and one
    /// newline -- `[INFO] <label> elapsed: <seconds>.<millis> seconds` -- with no
    /// escape sequence anywhere; on a terminal the very same bytes are written
    /// with only the severity tag painted.
    #[test]
    fn when_elapsed_report_is_written_given_redirected_stdout_then_preserves_exact_plain_line_and_newline()
     {
        let mut out = OutputCoordinator::new(RecordingSink::new());
        out.elapsed_log("bash", Duration::from_millis(1_234))
            .unwrap();

        assert_eq!(out.sink().stdout(), "[INFO] bash elapsed: 1.234 seconds\n");
        assert_eq!(out.sink().stderr(), "", "the timing line is a stdout log");
        assert_eq!(out.last(), Some(OutputKind::Log));
        assert!(
            !out.sink().stdout().contains('\u{1b}'),
            "a redirected elapsed line carries no ANSI"
        );

        let mut terminal = OutputCoordinator::new(RecordingSink::new().stdout_terminal(true));
        terminal
            .elapsed_log("bash", Duration::from_millis(1_234))
            .unwrap();
        assert_eq!(
            terminal.sink().stdout(),
            format!("{INFO_FG}[INFO]{STYLE_END} bash elapsed: 1.234 seconds\n")
        );
    }

    /// Pipeline phase timing stays a separate informational stdout line with
    /// the existing framing: the coordinator's blank line when the previous
    /// kind differs, one newline for the message, the severity marker painted
    /// on a terminal, and plain bytes on a redirected stream.
    #[test]
    fn when_pipeline_timing_is_reported_given_phase_completion_then_keeps_info_line_and_spacing() {
        let elapsed = Duration::from_millis(1_234);
        assert_eq!(
            elapsed_line("plan-agent ask", elapsed),
            "[INFO] plan-agent ask elapsed: 1.234 seconds"
        );

        let mut redirected = OutputCoordinator::new(RecordingSink::new());
        redirected.narration("# phase done").unwrap();
        redirected.elapsed_log("plan-agent ask", elapsed).unwrap();
        assert_eq!(
            redirected.sink().stdout(),
            "# phase done\n\n[INFO] plan-agent ask elapsed: 1.234 seconds\n",
            "a phase log after narration keeps the coordinator's separator and newline"
        );
        assert!(
            !redirected.sink().stdout().contains('\u{1b}'),
            "a redirected phase line stays plain"
        );

        let mut terminal = OutputCoordinator::new(RecordingSink::new().stdout_terminal(true));
        terminal.elapsed_log("plan-agent ask", elapsed).unwrap();
        assert_eq!(
            terminal.sink().stdout(),
            format!("{INFO_FG}[INFO]{STYLE_END} plan-agent ask elapsed: 1.234 seconds\n"),
            "only the severity tag is painted on a terminal"
        );
    }

    // -- Phase-aware review and summary spacing -----------------------------

    /// Core entry-point behavior: the complete transcript of a completed
    /// review followed by two summary report/tool-call pairs, byte for byte.
    #[test]
    fn when_reports_and_calls_are_spaced_given_review_and_summary_phases_then_preserve_semantic_exceptions()
     {
        let mut out = OutputCoordinator::new(RecordingSink::new());
        out.thinking_log(
            "giga",
            "review-model",
            "review",
            12_345,
            Duration::from_millis(1_234),
        )
        .unwrap();
        out.stdout_log("[INFO] review round 1 elapsed: 2.000 seconds")
            .unwrap();
        out.stdout_log("[INFO] review round 1 passed").unwrap();
        out.thinking_log(
            "smoll",
            "summary-model",
            "summary",
            23_456,
            Duration::from_millis(3_000),
        )
        .unwrap();
        out.tool_call("[1/100] (0.250s) $ js: return tools.diff();")
            .unwrap();
        out.thinking_log(
            "smoll",
            "summary-model",
            "summary",
            23_457,
            Duration::from_millis(4_000),
        )
        .unwrap();
        out.tool_call("[2/100] (0.100s) $ js: return tools.diff();")
            .unwrap();

        let expected = concat!(
            "#thinking (1.234s) giga model=review-model phase=review context=12,345 B\n",
            "\n",
            "[INFO] review round 1 elapsed: 2.000 seconds\n",
            "[INFO] review round 1 passed\n",
            "\n",
            "#thinking (3.000s) smoll model=summary-model phase=summary context=23,456 B\n",
            "[1/100] (0.250s) $ js: return tools.diff();\n",
            "#thinking (4.000s) smoll model=summary-model phase=summary context=23,457 B\n",
            "[2/100] (0.100s) $ js: return tools.diff();\n",
        );

        assert_eq!(out.sink().stdout(), expected);
        assert_eq!(out.sink().stderr(), "");
        assert_eq!(out.last(), Some(OutputKind::ToolCall));
    }

    /// Regression: the review report's separator belongs to information lines
    /// only. An untagged or differently tagged log that follows the report is
    /// an ordinary log, so the generic kind policy applies -- no separator from
    /// the log before it -- and its bytes and single terminator stay exact.
    #[test]
    fn when_non_information_log_follows_given_review_report_then_uses_generic_spacing() {
        let report = "#thinking (1.234s) giga model=review-model phase=review context=12,345 B";

        for payload in [
            "plain review note",
            "[WARN] review round 1 degraded",
            "[ERROR] review round 1 failed",
            "[DEBUG] review diagnostics",
        ] {
            let mut out = OutputCoordinator::new(RecordingSink::new());
            out.thinking_log(
                "giga",
                "review-model",
                "review",
                12_345,
                Duration::from_millis(1_234),
            )
            .unwrap();
            out.stdout_log(payload).unwrap();

            assert_eq!(
                out.sink().stdout(),
                format!("{report}\n{payload}\n"),
                "a non-information log after a review report uses generic same-kind spacing: {payload:?}"
            );
            assert_eq!(out.sink().stderr(), "", "a stdout log is not framed");
            assert_eq!(out.last(), Some(OutputKind::Log));

            // The non-information log is not part of the review information
            // block, so a following message of a different kind takes the
            // generic separator.
            out.narration("# after").unwrap();
            assert_eq!(
                out.sink().stdout(),
                format!("{report}\n{payload}\n\n# after\n"),
                "the generic kind separator still applies after a non-information log"
            );
        }
    }

    /// A skipped message is no message: it writes nothing, and the pending
    /// review transition still applies to the next successful write.
    #[test]
    fn when_output_is_skipped_given_active_review_context_then_preserves_spacing_context() {
        let report = "#thinking (1.234s) giga model=review-model phase=review context=12,345 B";
        let mut out = OutputCoordinator::new(RecordingSink::new());
        out.thinking_log(
            "giga",
            "review-model",
            "review",
            12_345,
            Duration::from_millis(1_234),
        )
        .unwrap();
        let written = out.sink().ops.len();

        for content in [None, Some(""), Some("  \t\n ")] {
            if let Some(text) = printable_narration(content) {
                out.narration(text).unwrap();
            }
        }

        assert_eq!(
            out.sink().ops.len(),
            written,
            "a skipped message must not write"
        );
        assert_eq!(out.last(), Some(OutputKind::Log));

        out.stdout_log("[INFO] review round 1 elapsed: 2.000 seconds")
            .unwrap();
        assert_eq!(
            out.sink().stdout(),
            format!("{report}\n\n[INFO] review round 1 elapsed: 2.000 seconds\n")
        );
    }

    /// Regression: repeated tool calls are not an alternating report/tool-call
    /// pair, so the second call ends the summary-pair context. The next summary
    /// report still sits adjacent to the call line it follows, and the call
    /// that follows that report is adjacent too, because every semantic report
    /// keeps its following call adjacent without re-arming the pair exception.
    #[test]
    fn when_summary_pairs_are_interrupted_given_repeated_tool_calls_then_uses_generic_spacing() {
        let mut out = OutputCoordinator::new(RecordingSink::new());
        out.thinking_log(
            "smoll",
            "summary-model",
            "summary",
            23_456,
            Duration::from_millis(3_000),
        )
        .unwrap();
        out.tool_call("[1/100] (0.250s) $ js: return tools.diff();")
            .unwrap();
        out.tool_call("[2/100] (0.100s) $ js: return tools.diff();")
            .unwrap();
        out.thinking_log(
            "smoll",
            "summary-model",
            "summary",
            23_457,
            Duration::from_millis(4_000),
        )
        .unwrap();
        out.tool_call("[3/100] (0.100s) $ js: return tools.diff();")
            .unwrap();

        assert_eq!(
            out.sink().stdout(),
            concat!(
                "#thinking (3.000s) smoll model=summary-model phase=summary context=23,456 B\n",
                "[1/100] (0.250s) $ js: return tools.diff();\n",
                "[2/100] (0.100s) $ js: return tools.diff();\n",
                "#thinking (4.000s) smoll model=summary-model phase=summary context=23,457 B\n",
                "[3/100] (0.100s) $ js: return tools.diff();\n",
            ),
            "a repeated call ends the pair context, but the semantic report still keeps its following call adjacent"
        );
        assert_eq!(out.sink().stderr(), "");
        assert_eq!(out.last(), Some(OutputKind::ToolCall));
    }

    /// Regression: a repeated summary report is not an alternating report/call
    /// pair, so it uses the generic rule between reports -- no separator
    /// because the kind is unchanged -- and it ends the summary-pair context.
    /// The call after it is still adjacent because it directly follows a
    /// semantic report, but that adjacency does not restore the pair.
    #[test]
    fn when_summary_report_is_repeated_given_active_summary_pairs_then_uses_generic_spacing() {
        let mut out = OutputCoordinator::new(RecordingSink::new());
        out.thinking_log(
            "smoll",
            "summary-model",
            "summary",
            23_456,
            Duration::from_millis(3_000),
        )
        .unwrap();
        out.tool_call("[1/100] (0.250s) $ js: return tools.diff();")
            .unwrap();
        out.thinking_log(
            "smoll",
            "summary-model",
            "summary",
            23_457,
            Duration::from_millis(4_000),
        )
        .unwrap();
        out.thinking_log(
            "smoll",
            "summary-model",
            "summary",
            23_458,
            Duration::from_millis(5_000),
        )
        .unwrap();
        out.tool_call("[2/100] (0.100s) $ js: return tools.diff();")
            .unwrap();

        assert_eq!(
            out.sink().stdout(),
            concat!(
                "#thinking (3.000s) smoll model=summary-model phase=summary context=23,456 B\n",
                "[1/100] (0.250s) $ js: return tools.diff();\n",
                "#thinking (4.000s) smoll model=summary-model phase=summary context=23,457 B\n",
                "#thinking (5.000s) smoll model=summary-model phase=summary context=23,458 B\n",
                "[2/100] (0.100s) $ js: return tools.diff();\n",
            ),
            "a repeated report ends the pair context, but the following call stays adjacent"
        );
        assert_eq!(out.sink().stderr(), "");
        assert_eq!(out.last(), Some(OutputKind::ToolCall));
    }

    /// Regression: a non-pair output ends the summary-pair context, so the
    /// messages around it fall back to the generic kind separator -- one blank
    /// line between different kinds, byte for byte.
    #[test]
    fn when_summary_pairs_are_interrupted_given_non_pair_output_then_uses_generic_spacing() {
        let mut out = OutputCoordinator::new(RecordingSink::new());
        out.thinking_log(
            "smoll",
            "summary-model",
            "summary",
            23_456,
            Duration::from_millis(3_000),
        )
        .unwrap();
        out.tool_call("[1/100] (0.250s) $ js: return tools.diff();")
            .unwrap();
        out.narration("# interruption").unwrap();
        out.tool_call("[2/100] (0.100s) $ js: return tools.diff();")
            .unwrap();

        assert_eq!(
            out.sink().stdout(),
            concat!(
                "#thinking (3.000s) smoll model=summary-model phase=summary context=23,456 B\n",
                "[1/100] (0.250s) $ js: return tools.diff();\n",
                "\n",
                "# interruption\n",
                "\n",
                "[2/100] (0.100s) $ js: return tools.diff();\n",
            ),
            "a non-pair output ends the summary-pair context and uses generic spacing"
        );
        assert_eq!(out.sink().stderr(), "");
        assert_eq!(out.last(), Some(OutputKind::ToolCall));
    }

    /// Regression: a non-pair output ends the summary-pair context. A later
    /// summary report cannot re-arm the exception, but it is still a semantic
    /// report, so the call directly after it is adjacent. The separator that
    /// would restore the alternating pair is not applied.
    #[test]
    fn when_a_summary_report_follows_an_interruption_given_generic_context_then_the_call_is_adjacent_without_rearming()
     {
        let mut out = OutputCoordinator::new(RecordingSink::new());
        out.thinking_log(
            "smoll",
            "summary-model",
            "summary",
            23_456,
            Duration::from_millis(3_000),
        )
        .unwrap();
        out.tool_call("[1/100] (0.250s) $ js: return tools.diff();")
            .unwrap();
        out.narration("# interruption").unwrap();
        out.thinking_log(
            "smoll",
            "summary-model",
            "summary",
            23_457,
            Duration::from_millis(4_000),
        )
        .unwrap();
        out.tool_call("[2/100] (0.100s) $ js: return tools.diff();")
            .unwrap();

        assert_eq!(
            out.sink().stdout(),
            concat!(
                "#thinking (3.000s) smoll model=summary-model phase=summary context=23,456 B\n",
                "[1/100] (0.250s) $ js: return tools.diff();\n",
                "\n",
                "# interruption\n",
                "\n",
                "#thinking (4.000s) smoll model=summary-model phase=summary context=23,457 B\n",
                "[2/100] (0.100s) $ js: return tools.diff();\n",
            ),
            "a summary report after an interruption keeps its call adjacent without re-arming the pair exception"
        );
        assert_eq!(out.sink().stderr(), "");
        assert_eq!(out.last(), Some(OutputKind::ToolCall));
    }

    /// Contract: a failed review-report write commits no review transition,
    /// so the next successful log is spaced from the last written message.
    #[test]
    fn when_review_report_write_fails_given_existing_output_then_preserves_spacing_context() {
        let mut out = OutputCoordinator::new(RecordingSink::new());
        out.stdout_log("[INFO] existing").unwrap();
        assert_eq!(out.last(), Some(OutputKind::Log));

        out.sink_mut().fail = true;
        assert!(
            out.thinking_log(
                "giga",
                "review-model",
                "review",
                12_345,
                Duration::from_millis(1_234),
            )
            .is_err()
        );
        assert_eq!(out.sink().stdout(), "[INFO] existing\n");
        assert_eq!(out.last(), Some(OutputKind::Log));

        out.sink_mut().fail = false;
        out.stdout_log("[INFO] after").unwrap();
        assert_eq!(
            out.sink().stdout(),
            "[INFO] existing\n[INFO] after\n",
            "the failed report must not have left a review separator behind"
        );
    }

    /// Contract: a failed summary-report flush commits no summary transition,
    /// so the next successful summary report still gets the review-to-summary
    /// separator.
    #[test]
    fn when_summary_report_flush_fails_given_active_review_context_then_preserves_spacing_context()
    {
        let mut out = OutputCoordinator::new(RecordingSink::new());
        out.thinking_log(
            "giga",
            "review-model",
            "review",
            12_345,
            Duration::from_millis(1_234),
        )
        .unwrap();
        out.stdout_log("[INFO] review round 1 elapsed: 2.000 seconds")
            .unwrap();
        out.stdout_log("[INFO] review round 1 passed").unwrap();
        let reviewed = out.sink().stdout();

        out.sink_mut().fail_flush = true;
        assert!(
            out.thinking_log(
                "smoll",
                "summary-model",
                "summary",
                23_456,
                Duration::from_millis(3_000),
            )
            .is_err()
        );
        assert_eq!(out.last(), Some(OutputKind::Log));

        out.sink_mut().fail_flush = false;
        out.thinking_log(
            "smoll",
            "summary-model",
            "summary",
            23_457,
            Duration::from_millis(4_000),
        )
        .unwrap();
        out.tool_call("[1/100] (0.250s) $ js: return tools.diff();")
            .unwrap();

        let failed = "#thinking (3.000s) smoll model=summary-model phase=summary context=23,456 B";
        let succeeded =
            "#thinking (4.000s) smoll model=summary-model phase=summary context=23,457 B";
        assert_eq!(
            out.sink().stdout(),
            format!(
                "{reviewed}\n{failed}\n\n{succeeded}\n\
                 [1/100] (0.250s) $ js: return tools.diff();\n"
            ),
            "the failed summary report must not commit the summary transition"
        );
    }

    /// Consolidated edge-case table for the phase-spacing contract: every
    /// parameterizable boundary -- zero metadata, the minimum valid report,
    /// the largest context, unbounded consecutive summary pairs, and
    /// noncanonical phases and ordinary payloads that must fall back to the
    /// generic policy -- driven through the coordinator's public methods and
    /// asserted against the exact caller-visible stdout bytes.
    #[test]
    fn when_output_boundaries_are_exercised_given_edge_case_table_then_preserves_contract() {
        /// One caller-visible action in a transcript.
        #[derive(Clone, Copy)]
        enum Step {
            Report {
                provider: &'static str,
                model: &'static str,
                phase: &'static str,
                context_bytes: usize,
                millis: u64,
            },
            StdoutLog(&'static str),
            ToolCall(&'static str),
            Narration(&'static str),
        }

        /// One boundary row: the actions, the exact stdout bytes they must
        /// produce, and the kind the coordinator must remember afterwards.
        struct Case {
            boundary: String,
            steps: Vec<Step>,
            expected_stdout: String,
            expected_last: OutputKind,
        }

        let mut cases: Vec<Case> = vec![

        // Empty/zero boundary: no elapsed time and no context still render
        // their exact values, and the review block keeps its separator.
        Case {
            boundary: "a zero duration and zero context report keeps 0.000s and 0 B with the review spacing"
                .to_string(),
            steps: vec![
                Step::Report {
                    provider: "mid",
                    model: "zero-model",
                    phase: "review",
                    context_bytes: 0,
                    millis: 0,
                },
                Step::StdoutLog("[INFO] review round 1 elapsed: 0.000 seconds"),
                Step::StdoutLog("[INFO] review round 1 passed"),
            ],
            expected_stdout: concat!(
                "#thinking (0.000s) mid model=zero-model phase=review context=0 B\n",
                "\n",
                "[INFO] review round 1 elapsed: 0.000 seconds\n",
                "[INFO] review round 1 passed\n",
            )
            .to_string(),
            expected_last: OutputKind::Log,
        },

        // Minimum valid boundary: one byte and one millisecond are reported
        // as themselves, and the review block still gets one blank line.
        Case {
            boundary: "the minimum valid report keeps one byte and one millisecond with the review spacing"
                .to_string(),
            steps: vec![
                Step::Report {
                    provider: "giga",
                    model: "min-model",
                    phase: "review",
                    context_bytes: 1,
                    millis: 1,
                },
                Step::StdoutLog("[INFO] review round 1 passed"),
            ],
            expected_stdout: concat!(
                "#thinking (0.001s) giga model=min-model phase=review context=1 B\n",
                "\n",
                "[INFO] review round 1 passed\n",
            )
            .to_string(),
            expected_last: OutputKind::Log,
        },

        // Minimum valid summary pair: the report and its call line are
        // adjacent, with no separator between them.
        Case {
            boundary: "the minimum valid summary pair stays adjacent".to_string(),
            steps: vec![
                Step::Report {
                    provider: "smoll",
                    model: "summary-model",
                    phase: "summary",
                    context_bytes: 1,
                    millis: 1,
                },
                Step::ToolCall("[1/100] (0.250s) $ js: return tools.diff();"),
            ],
            expected_stdout: concat!(
                "#thinking (0.001s) smoll model=summary-model phase=summary context=1 B\n",
                "[1/100] (0.250s) $ js: return tools.diff();\n",
            )
            .to_string(),
            expected_last: OutputKind::ToolCall,
        },

        // Interruption boundary: a non-pair output ends the summary-pair
        // context. A later summary report cannot restart the exception, but it
        // still keeps its directly following call line adjacent.
        Case {
            boundary: "an interrupted summary sequence does not re-arm on a later report"
                .to_string(),
            steps: vec![
                Step::Report {
                    provider: "smoll",
                    model: "summary-model",
                    phase: "summary",
                    context_bytes: 23_456,
                    millis: 3_000,
                },
                Step::ToolCall("[1/100] (0.250s) $ js: return tools.diff();"),
                Step::Narration("# interruption"),
                Step::Report {
                    provider: "smoll",
                    model: "summary-model",
                    phase: "summary",
                    context_bytes: 23_457,
                    millis: 4_000,
                },
                Step::ToolCall("[2/100] (0.100s) $ js: return tools.diff();"),
            ],
            expected_stdout: concat!(
                "#thinking (3.000s) smoll model=summary-model phase=summary context=23,456 B\n",
                "[1/100] (0.250s) $ js: return tools.diff();\n",
                "\n",
                "# interruption\n",
                "\n",
                "#thinking (4.000s) smoll model=summary-model phase=summary context=23,457 B\n",
                "[2/100] (0.100s) $ js: return tools.diff();\n",
            )
            .to_string(),
            expected_last: OutputKind::ToolCall,
        },

        // Maximum limit: the largest possible context keeps its grouped
        // digits, untruncated and unrewritten, and the review spacing follows.
        Case {
            boundary: "the largest context keeps 18,446,744,073,709,551,615 B with the review spacing"
                .to_string(),
            steps: vec![
                Step::Report {
                    provider: "giga",
                    model: "max-model",
                    phase: "review",
                    context_bytes: usize::MAX,
                    millis: 1_234,
                },
                Step::StdoutLog("[INFO] review round 1 passed"),
            ],
            expected_stdout: concat!(
                "#thinking (1.234s) giga model=max-model phase=review context=18,446,744,073,709,551,615 B\n",
                "\n",
                "[INFO] review round 1 passed\n",
            )
            .to_string(),
            expected_last: OutputKind::Log,
        },

        // There is no configured maximum number of transcript pairs: three
        // consecutive report/tool-call pairs remain one adjacent block.
        Case {
            boundary: "consecutive summary pairs have no maximum and stay adjacent".to_string(),
            steps: vec![
                Step::Report {
                    provider: "smoll",
                    model: "summary-model",
                    phase: "summary",
                    context_bytes: 23_456,
                    millis: 3_000,
                },
                Step::ToolCall("[1/100] (0.250s) $ js: return tools.diff();"),
                Step::Report {
                    provider: "smoll",
                    model: "summary-model",
                    phase: "summary",
                    context_bytes: 23_457,
                    millis: 4_000,
                },
                Step::ToolCall("[2/100] (0.100s) $ js: return tools.diff();"),
                Step::Report {
                    provider: "smoll",
                    model: "summary-model",
                    phase: "summary",
                    context_bytes: 23_458,
                    millis: 5_000,
                },
                Step::ToolCall("[3/100] (0.050s) $ js: return tools.diff();"),
            ],
            expected_stdout: concat!(
                "#thinking (3.000s) smoll model=summary-model phase=summary context=23,456 B\n",
                "[1/100] (0.250s) $ js: return tools.diff();\n",
                "#thinking (4.000s) smoll model=summary-model phase=summary context=23,457 B\n",
                "[2/100] (0.100s) $ js: return tools.diff();\n",
                "#thinking (5.000s) smoll model=summary-model phase=summary context=23,458 B\n",
                "[3/100] (0.050s) $ js: return tools.diff();\n",
            )
            .to_string(),
            expected_last: OutputKind::ToolCall,
        },
        ];

        // Malformed/invalid boundary: an unknown or differently cased phase
        // is still submitted through the semantic report route, so its payload
        // bytes stay exact and the call directly after it is adjacent.
        for phase in [
            "Review", "REVIEW", "Summary", "SUMMARY", "review ", "code", "unknown",
        ] {
            cases.push(Case {
                boundary: format!(
                    "a noncanonical phase {phase:?} keeps its payload and keeps the following call adjacent"
                ),
                steps: vec![
                    Step::Narration("# before"),
                    Step::Report {
                        provider: "mid",
                        model: "example-model",
                        phase,
                        context_bytes: 1,
                        millis: 1,
                    },
                    Step::ToolCall("$ after"),
                ],
                expected_stdout: format!(
                    "# before\n\n#thinking (0.001s) mid model=example-model phase={phase} context=1 B\n$ after\n"
                ),
                expected_last: OutputKind::ToolCall,
            });
        }

        // Ordinary logs carry no phase context: same-kind messages stay
        // adjacent and a different kind gets one blank line, byte for byte.
        cases.push(Case {
            boundary:
                "ordinary logs without a phase report keep their bytes and the generic spacing"
                    .to_string(),
            steps: vec![
                Step::Narration("# before"),
                Step::StdoutLog("plain message"),
                Step::StdoutLog("[INFO] message"),
                Step::ToolCall("$ after"),
            ],
            expected_stdout: concat!(
                "# before\n",
                "\n",
                "plain message\n",
                "[INFO] message\n",
                "\n",
                "$ after\n",
            )
            .to_string(),
            expected_last: OutputKind::ToolCall,
        });

        // Malformed payload: an unrecognized report-shaped line is still an
        // ordinary log, so its bytes survive and the generic spacing applies.
        cases.push(Case {
            boundary: "an ordinary malformed payload keeps its bytes and the generic spacing"
                .to_string(),
            steps: vec![
                Step::Narration("# before"),
                Step::StdoutLog("#thinking (broken phase=review"),
                Step::StdoutLog("[INFO] done"),
                Step::ToolCall("$ after"),
            ],
            expected_stdout: concat!(
                "# before\n",
                "\n",
                "#thinking (broken phase=review\n",
                "[INFO] done\n",
                "\n",
                "$ after\n",
            )
            .to_string(),
            expected_last: OutputKind::ToolCall,
        });

        for case in cases {
            let mut out = OutputCoordinator::new(RecordingSink::new());
            for step in case.steps {
                match step {
                    Step::Report {
                        provider,
                        model,
                        phase,
                        context_bytes,
                        millis,
                    } => out
                        .thinking_log(
                            provider,
                            model,
                            phase,
                            context_bytes,
                            Duration::from_millis(millis),
                        )
                        .unwrap(),
                    Step::StdoutLog(payload) => out.stdout_log(payload).unwrap(),
                    Step::ToolCall(payload) => out.tool_call(payload).unwrap(),
                    Step::Narration(payload) => out.narration(payload).unwrap(),
                }
            }

            assert_eq!(
                out.sink().stdout(),
                case.expected_stdout,
                "{}",
                case.boundary
            );
            assert_eq!(out.sink().stderr(), "", "{}", case.boundary);
            assert_eq!(out.last(), Some(case.expected_last), "{}", case.boundary);
            assert!(
                !out.sink().stdout().contains('\u{1b}'),
                "{} must stay plain",
                case.boundary
            );
        }
    }

    // -- Thinking-report adjacency after a call line ------------------------

    /// Core entry-point behavior: a semantic thinking report immediately after
    /// a successfully coordinated call line is written on the next line with
    /// no intervening blank line. Both payloads keep their own terminating
    /// newline, and the redirected stream stays free of escape sequences.
    #[test]
    fn when_a_call_line_is_followed_by_a_thinking_report_given_redirected_output_then_writes_exact_adjacent_lines()
     {
        let call = "[1/100] (0.250s) $ cargo test";
        let report = "#thinking (1.234s) mid model=example-model phase=code context=12,345 B";
        let mut out = OutputCoordinator::new(RecordingSink::new());
        out.tool_call(call).unwrap();
        out.thinking_log(
            "mid",
            "example-model",
            "code",
            12_345,
            Duration::from_millis(1_234),
        )
        .unwrap();

        assert_eq!(out.sink().stdout(), format!("{call}\n{report}\n"));
        assert_eq!(out.sink().stderr(), "", "the report is stdout output");
        assert_eq!(out.last(), Some(OutputKind::Log));
        assert_eq!(
            out.sink().ops,
            [
                Op::Stdout(format!("{call}\n")),
                Op::FlushStdout,
                Op::Stdout(format!("{report}\n")),
                Op::FlushStdout,
            ],
            "the report is written and flushed exactly like a log"
        );
        assert!(
            out.sink().stdout().ends_with('\n'),
            "the report keeps its terminating newline"
        );
        assert!(
            !out.sink().stdout().contains("\n\n"),
            "there is no blank separator between the call and the report"
        );
        assert!(
            !out.sink().stdout().contains('\u{1b}'),
            "a redirected transcript stays plain"
        );
    }

    /// Core entry-point behavior: on terminal stdout each whole payload keeps
    /// its cyan wrapper and full reset, while the call's newline, the report's
    /// terminating newline and the (empty) separator remain uncolored. The
    /// report starts immediately after the call's newline.
    #[test]
    fn when_a_call_line_is_followed_by_a_thinking_report_given_terminal_stdout_then_colors_only_payloads()
     {
        let call = "[1/100] (0.250s) $ cargo test";
        let report = "#thinking (1.234s) mid model=example-model phase=code context=12,345 B";
        let mut out = OutputCoordinator::new(RecordingSink::new().stdout_terminal(true));
        out.tool_call(call).unwrap();
        out.thinking_log(
            "mid",
            "example-model",
            "code",
            12_345,
            Duration::from_millis(1_234),
        )
        .unwrap();

        let expected = format!("{CYAN}{call}{STYLE_END}\n{CYAN}{report}{STYLE_END}\n");
        assert_eq!(
            out.sink().stdout(),
            expected,
            "each whole payload keeps its cyan and reset, and both newlines stay plain"
        );
        assert!(
            out.sink()
                .stdout()
                .contains(&format!("{STYLE_END}\n{CYAN}")),
            "the call's reset and newline sit right before the report's cyan"
        );
        assert!(
            !out.sink().stdout().contains("\n\n"),
            "the report begins immediately after the call's newline"
        );
        assert_eq!(out.sink().stderr(), "", "the report is stdout output");
        assert_eq!(out.last(), Some(OutputKind::Log));
    }

    /// Edge-case table: every canonical report phase, and a noncanonical one,
    /// is adjacent when it follows a call line, because report eligibility is
    /// the semantic reporting path rather than the phase spelling.
    #[test]
    fn when_a_call_line_is_followed_by_a_thinking_report_given_each_report_phase_then_writes_adjacent_lines()
     {
        let call = "[1/100] (0.250s) $ cargo test";
        for phase in [
            "plan", "spec", "tickets", "triage", "code", "review", "summary", "custom",
        ] {
            let report =
                format!("#thinking (0.001s) mid model=example-model phase={phase} context=1 B");
            let mut out = OutputCoordinator::new(RecordingSink::new());
            out.tool_call(call).unwrap();
            out.thinking_log("mid", "example-model", phase, 1, Duration::from_millis(1))
                .unwrap();

            assert_eq!(
                out.sink().stdout(),
                format!("{call}\n{report}\n"),
                "phase {phase:?} must be adjacent to the call line"
            );
            assert!(
                !out.sink().stdout().contains("\n\n"),
                "phase {phase:?} must have no intervening blank line"
            );
        }

        // Adjacency changes no phase-aware transition: a review report after a
        // call still opens the review information block, so the first INFO
        // line after it keeps its one blank line.
        let mut out = OutputCoordinator::new(RecordingSink::new());
        out.tool_call(call).unwrap();
        out.thinking_log(
            "giga",
            "review-model",
            "review",
            12_345,
            Duration::from_millis(1_234),
        )
        .unwrap();
        out.stdout_log("[INFO] review round 1 passed").unwrap();
        assert_eq!(
            out.sink().stdout(),
            format!(
                "{call}\n#thinking (1.234s) giga model=review-model phase=review context=12,345 B\n\n[INFO] review round 1 passed\n"
            ),
            "the review separator belongs to the information line, not the report"
        );
    }

    /// Empty/zero boundary: zero duration and zero context keep their exact
    /// fields, the adjacency, and both messages' single terminating newline.
    #[test]
    fn when_a_call_line_is_followed_by_a_thinking_report_given_zero_duration_and_context_then_keeps_both_newlines()
     {
        let call = "[1/100] (0.000s) $ true";
        let report = "#thinking (0.000s) mid model=zero-model phase=code context=0 B";
        let mut out = OutputCoordinator::new(RecordingSink::new());
        out.tool_call(call).unwrap();
        out.thinking_log("mid", "zero-model", "code", 0, Duration::ZERO)
            .unwrap();

        assert_eq!(out.sink().stdout(), format!("{call}\n{report}\n"));
        assert_eq!(
            out.sink().stdout().matches('\n').count(),
            2,
            "each message keeps exactly one terminator"
        );
        assert!(out.sink().stdout().starts_with(&format!("{call}\n")));
        assert!(out.sink().stdout().ends_with(&format!("{report}\n")));
        assert!(!out.sink().stdout().contains("\n\n"));
        assert_eq!(out.last(), Some(OutputKind::Log));
    }

    /// Minimum valid boundary: the shortest production call line followed by
    /// the minimum report keeps the exact 0.001s and 1 B fields without a blank
    /// separator.
    #[test]
    fn when_a_call_line_is_followed_by_a_thinking_report_given_the_shortest_call_and_minimum_report_then_preserves_exact_fields()
     {
        let call = call_line(1, 1, "", None);
        assert_eq!(call, "[1/1] $", "the shortest call line is exactly this");
        let report = "#thinking (0.001s) giga model=min-model phase=review context=1 B";
        let mut out = OutputCoordinator::new(RecordingSink::new());
        out.tool_call(&call).unwrap();
        out.thinking_log("giga", "min-model", "review", 1, Duration::from_millis(1))
            .unwrap();

        assert_eq!(out.sink().stdout(), format!("{call}\n{report}\n"));
        assert!(out.sink().stdout().contains("(0.001s)"));
        assert!(out.sink().stdout().contains("context=1 B"));
        assert!(!out.sink().stdout().contains("\n\n"));
        assert_eq!(out.last(), Some(OutputKind::Log));
    }

    /// Maximum limit: a production-built call line capped at the line budget
    /// stays capped, while Duration::MAX and usize::MAX report their complete
    /// grouped metadata and stay adjacent.
    #[test]
    fn when_a_call_line_is_followed_by_a_thinking_report_given_maximum_call_and_report_values_then_preserves_cap_and_metadata()
     {
        let display = "x".repeat(MAX_CALL_LINE * 4);
        let call = call_line(100, 100, &display, Some(Duration::from_millis(250)));
        assert_eq!(
            call.chars().count(),
            MAX_CALL_LINE,
            "the call line is capped at the production limit"
        );
        assert!(call.ends_with("..."), "the capped line keeps its ellipsis");

        let mut out = OutputCoordinator::new(RecordingSink::new());
        out.tool_call(&call).unwrap();
        out.thinking_log("mid", "max-model", "review", usize::MAX, Duration::MAX)
            .unwrap();

        assert_eq!(
            out.sink().stdout(),
            format!("{call}\n{MAXIMUM_REPORT}\n"),
            "the capped call and complete maximum report are adjacent"
        );
        assert!(out.sink().stdout().contains("(18446744073709551615.999s)"));
        assert!(
            out.sink()
                .stdout()
                .contains("context=18,446,744,073,709,551,615 B")
        );
        assert!(!out.sink().stdout().contains("\n\n"));
        assert_eq!(out.last(), Some(OutputKind::Log));
    }

    /// Malformed/invalid boundary: a thinking-shaped lookalike submitted as an
    /// ordinary log is not a semantic report, so it keeps the generic blank
    /// line after a call line.
    #[test]
    fn when_an_ordinary_log_follows_a_call_line_given_thinking_shaped_text_then_keeps_generic_spacing()
     {
        let call = "[1/100] (0.250s) $ cargo test";
        for payload in [
            "#thinking custom text",
            "#thinking (1.234s) mid model=example-model phase=code context=12,345 B",
        ] {
            let mut out = OutputCoordinator::new(RecordingSink::new());
            out.tool_call(call).unwrap();
            out.stdout_log(payload).unwrap();

            assert_eq!(
                out.sink().stdout(),
                format!("{call}\n\n{payload}\n"),
                "a lookalike log keeps the generic separator: {payload:?}"
            );
            assert_eq!(out.last(), Some(OutputKind::Log));
        }
    }

    /// Malformed/invalid boundary: a noncanonical phase still goes through the
    /// semantic report path, so its spelling does not disqualify it from the
    /// adjacency exception.
    #[test]
    fn when_a_noncanonical_report_follows_a_call_line_given_noncanonical_phase_then_writes_adjacent_lines()
     {
        let call = "[1/100] (0.250s) $ cargo test";
        let report = "#thinking (0.001s) mid model=example-model phase=custom context=1 B";
        let mut out = OutputCoordinator::new(RecordingSink::new());
        out.tool_call(call).unwrap();
        out.thinking_log(
            "mid",
            "example-model",
            "custom",
            1,
            Duration::from_millis(1),
        )
        .unwrap();

        assert_eq!(out.sink().stdout(), format!("{call}\n{report}\n"));
        assert!(!out.sink().stdout().contains("\n\n"));
        assert_eq!(out.last(), Some(OutputKind::Log));
    }

    /// Contract: a failed write or flush surfaces the existing I/O error and
    /// commits neither the report's kind nor its spacing transition, so the
    /// next separator is chosen from the last successfully committed state.
    #[test]
    fn when_a_thinking_report_fails_given_write_or_flush_error_then_preserves_last_successful_spacing_state()
     {
        let call = "[1/100] (0.250s) $ cargo test";
        let failed_report =
            "#thinking (1.234s) mid model=example-model phase=code context=12,345 B";
        let next_report = "#thinking (0.002s) mid model=example-model phase=code context=12,346 B";

        // A failed write emits no report bytes and leaves the call line as the
        // last successful state, so the next report is still adjacent.
        let mut write_failed = OutputCoordinator::new(RecordingSink::new());
        write_failed.tool_call(call).unwrap();
        write_failed.sink_mut().fail = true;
        let error = write_failed
            .thinking_log(
                "mid",
                "example-model",
                "code",
                12_345,
                Duration::from_millis(1_234),
            )
            .unwrap_err();
        assert_eq!(error.to_string(), "stdout write failed");
        assert_eq!(write_failed.sink().stdout(), format!("{call}\n"));
        assert_eq!(write_failed.last(), Some(OutputKind::ToolCall));

        write_failed.sink_mut().fail = false;
        write_failed
            .thinking_log(
                "mid",
                "example-model",
                "code",
                12_346,
                Duration::from_millis(2),
            )
            .unwrap();
        assert_eq!(
            write_failed.sink().stdout(),
            format!("{call}\n{next_report}\n"),
            "the next report still uses the last committed call line"
        );

        // A failed flush keeps the same kind and transition: the call line
        // remains the last successful state even though the report bytes
        // reached the sink.
        let mut flush_failed = OutputCoordinator::new(RecordingSink::new());
        flush_failed.tool_call(call).unwrap();
        flush_failed.sink_mut().fail_flush = true;
        let error = flush_failed
            .thinking_log(
                "mid",
                "example-model",
                "code",
                12_345,
                Duration::from_millis(1_234),
            )
            .unwrap_err();
        assert_eq!(error.to_string(), "stdout flush failed");
        assert_eq!(
            flush_failed.sink().stdout(),
            format!("{call}\n{failed_report}\n")
        );
        assert_eq!(flush_failed.last(), Some(OutputKind::ToolCall));

        flush_failed.sink_mut().fail_flush = false;
        flush_failed
            .thinking_log(
                "mid",
                "example-model",
                "code",
                12_346,
                Duration::from_millis(2),
            )
            .unwrap();
        assert_eq!(
            flush_failed.sink().stdout(),
            format!("{call}\n{failed_report}\n{next_report}\n"),
            "the next report still uses the last committed call line"
        );

        // With narration as the last successful state, a failed report leaves
        // it in place, so the next report keeps the generic blank separator.
        let mut after_narration = OutputCoordinator::new(RecordingSink::new());
        after_narration.narration("# existing").unwrap();
        after_narration.sink_mut().fail = true;
        assert!(
            after_narration
                .thinking_log(
                    "mid",
                    "example-model",
                    "code",
                    12_345,
                    Duration::from_millis(1_234),
                )
                .is_err()
        );
        assert_eq!(after_narration.last(), Some(OutputKind::Narration));

        after_narration.sink_mut().fail = false;
        after_narration
            .thinking_log(
                "mid",
                "example-model",
                "code",
                12_346,
                Duration::from_millis(2),
            )
            .unwrap();
        assert_eq!(
            after_narration.sink().stdout(),
            format!("# existing\n\n{next_report}\n"),
            "the failed report must not commit a call-line adjacency"
        );
    }

    /// Regression: a call line followed by narration still gets the generic
    /// blank-line separator; only the tool-call-to-report pair is adjacent.
    #[test]
    fn when_a_call_line_is_followed_by_narration_given_different_output_kinds_then_keeps_generic_spacing()
     {
        let mut out = OutputCoordinator::new(RecordingSink::new());
        out.tool_call("[1/100] (0.250s) $ cargo test").unwrap();
        out.narration("# continuing").unwrap();

        assert_eq!(
            out.sink().stdout(),
            "[1/100] (0.250s) $ cargo test\n\n# continuing\n"
        );
        assert_eq!(out.last(), Some(OutputKind::Narration));
    }

    /// Regression: a thinking report after narration keeps the generic
    /// blank-line separator.
    #[test]
    fn when_a_report_follows_narration_given_different_output_kinds_then_keeps_generic_spacing() {
        let report = "#thinking (1.234s) mid model=example-model phase=code context=12,345 B";
        let mut out = OutputCoordinator::new(RecordingSink::new());
        out.narration("# done").unwrap();
        out.thinking_log(
            "mid",
            "example-model",
            "code",
            12_345,
            Duration::from_millis(1_234),
        )
        .unwrap();

        assert_eq!(out.sink().stdout(), format!("# done\n\n{report}\n"));
        assert_eq!(out.last(), Some(OutputKind::Log));
    }

    // -- Thinking-report colouring at the coordinator boundary --------------

    /// The exact payloads at the three report boundaries, kept as literals so
    /// a change to report formatting or byte grouping is a deliberate edit.
    const ZERO_REPORT: &str = "#thinking (0.000s) mid model=zero-model phase=code context=0 B";
    const MINIMUM_REPORT: &str = "#thinking (0.001s) giga model=min-model phase=review context=1 B";
    const MAXIMUM_REPORT: &str = "#thinking (18446744073709551615.999s) mid model=max-model phase=review context=18,446,744,073,709,551,615 B";

    /// The bytes a report becomes on a terminal or a redirected stdout.
    fn report_bytes(payload: &str, terminal: bool) -> String {
        if terminal {
            format!("{CYAN}{payload}{STYLE_END}\n")
        } else {
            format!("{payload}\n")
        }
    }

    /// Core entry-point behavior: on terminal stdout the complete report is
    /// painted in the palette cyan that a complete call line uses, the full
    /// reset lands immediately after the payload, and the separator and
    /// newline around it stay plain.
    #[test]
    fn when_model_attempt_is_reported_given_terminal_stdout_then_cyan_wraps_the_whole_report() {
        let report = "#thinking (1.234s) mid model=example-model phase=code context=12,345 B";
        let mut out = OutputCoordinator::new(RecordingSink::new().stdout_terminal(true));
        out.narration("# before").unwrap();
        out.thinking_log(
            "mid",
            "example-model",
            "code",
            12_345,
            Duration::from_millis(1_234),
        )
        .unwrap();

        assert_eq!(
            out.sink().stdout(),
            format!("# before\n\n{CYAN}{report}{STYLE_END}\n"),
            "the plain separator precedes the whole-payload cyan and the reset precedes the newline"
        );
        assert!(
            out.sink()
                .stdout()
                .contains(&format!("{report}{STYLE_END}\n")),
            "the reset must arrive immediately after the complete payload"
        );
        assert_eq!(out.sink().stderr(), "", "the report is stdout output");
        assert_eq!(out.last(), Some(OutputKind::Log));
        assert!(
            !out.sink().stdout().contains(INFO_FG),
            "a report never takes the severity marker colour"
        );
    }

    /// Core entry-point behavior: redirected or piped stdout gets exactly the
    /// plain report payload and one newline, with no escape sequence.
    #[test]
    fn when_model_attempt_is_reported_given_redirected_stdout_then_report_has_no_ansi() {
        let report = "#thinking (1.234s) mid model=example-model phase=code context=12,345 B";
        let mut out = OutputCoordinator::new(RecordingSink::new());
        out.thinking_log(
            "mid",
            "example-model",
            "code",
            12_345,
            Duration::from_millis(1_234),
        )
        .unwrap();

        assert_eq!(out.sink().stdout(), format!("{report}\n"));
        assert_eq!(out.sink().stderr(), "", "the report is stdout output");
        assert_eq!(out.last(), Some(OutputKind::Log));
        assert!(
            !out.sink().stdout().contains('\u{1b}'),
            "a redirected report carries no ANSI"
        );
    }

    /// Edge-case table: zero, one millisecond/one byte and the largest
    /// duration/context all keep their complete payload through both the
    /// terminal and the redirected contract.
    #[test]
    fn when_report_boundaries_are_exercised_given_zero_minimum_and_maximum_values_then_payload_is_preserved()
     {
        let cases = [
            (
                "mid",
                "zero-model",
                "code",
                0usize,
                Duration::ZERO,
                ZERO_REPORT,
            ),
            (
                "giga",
                "min-model",
                "review",
                1usize,
                Duration::from_millis(1),
                MINIMUM_REPORT,
            ),
            (
                "mid",
                "max-model",
                "review",
                usize::MAX,
                Duration::MAX,
                MAXIMUM_REPORT,
            ),
        ];

        for terminal in [false, true] {
            for (provider, model, phase, context_bytes, elapsed, payload) in cases {
                let mut out =
                    OutputCoordinator::new(RecordingSink::new().stdout_terminal(terminal));
                out.thinking_log(provider, model, phase, context_bytes, elapsed)
                    .unwrap();

                assert_eq!(
                    out.sink().stdout(),
                    report_bytes(payload, terminal),
                    "boundary payload {payload:?} on terminal={terminal}"
                );
                assert_eq!(out.sink().stderr(), "", "boundary payload {payload:?}");
                assert_eq!(
                    out.last(),
                    Some(OutputKind::Log),
                    "boundary payload {payload:?}"
                );
            }
        }
    }

    /// Malformed/invalid boundary: a noncanonical phase is preserved exactly
    /// and still takes the report cyan, because report identity comes from the
    /// reporting path rather than from the phase text.
    #[test]
    fn when_model_attempt_is_reported_given_noncanonical_phase_then_phase_text_is_preserved_and_cyan()
     {
        for phase in ["Review", "unknown"] {
            let report =
                format!("#thinking (0.001s) mid model=example-model phase={phase} context=1 B");
            let mut out = OutputCoordinator::new(RecordingSink::new().stdout_terminal(true));
            out.thinking_log("mid", "example-model", phase, 1, Duration::from_millis(1))
                .unwrap();

            assert_eq!(
                out.sink().stdout(),
                report_bytes(&report, true),
                "phase {phase:?} is neither normalized nor left plain"
            );
            assert_eq!(out.last(), Some(OutputKind::Log));
        }
    }

    /// Malformed/invalid boundary: a generic log whose text only looks like a
    /// report is not submitted through the report path, so it stays byte for
    /// byte unstyled on a terminal.
    #[test]
    fn when_thinking_shaped_log_is_written_given_terminal_stdout_then_remains_unstyled() {
        let lookalike = "#thinking (1.234s) mid model=example-model phase=code context=12,345 B";
        let mut out = OutputCoordinator::new(RecordingSink::new().stdout_terminal(true));
        out.stdout_log(lookalike).unwrap();

        assert_eq!(out.sink().stdout(), format!("{lookalike}\n"));
        assert_eq!(out.last(), Some(OutputKind::Log));
        assert!(
            !out.sink().stdout().contains('\u{1b}'),
            "a prefix lookalike is an ordinary log"
        );
    }

    /// Contract: the terminal decision is stdout's own capability. A terminal
    /// stderr must not enable colour for a redirected stdout.
    #[test]
    fn when_a_report_is_written_given_terminal_stderr_and_redirected_stdout_then_keeps_the_report_plain()
     {
        let report = "#thinking (1.234s) mid model=example-model phase=code context=12,345 B";
        let mut out = OutputCoordinator::new(
            RecordingSink::new()
                .stdout_terminal(false)
                .stderr_terminal(true),
        );
        out.thinking_log(
            "mid",
            "example-model",
            "code",
            12_345,
            Duration::from_millis(1_234),
        )
        .unwrap();
        out.stderr_log("[WARN] diagnostic").unwrap();

        assert_eq!(out.sink().stdout(), format!("{report}\n"));
        assert!(
            !out.sink().stdout().contains('\u{1b}'),
            "a redirected stdout stays plain even when stderr is a terminal"
        );
        assert_eq!(
            out.sink().stderr(),
            format!("{WARN_FG}[WARN]{STYLE_END} diagnostic\n"),
            "stderr keeps its own independent rendering"
        );
        assert_eq!(out.last(), Some(OutputKind::Log));
    }

    /// Contract: the report keeps the established log result, stdout routing,
    /// separator, one-newline framing and stdout flush.
    #[test]
    fn when_model_attempt_is_reported_given_successful_stdout_then_remains_a_flushed_log_with_existing_spacing()
     {
        let report = "#thinking (1.234s) mid model=example-model phase=code context=12,345 B";
        let mut out = OutputCoordinator::new(RecordingSink::new());
        out.narration("# before").unwrap();
        out.thinking_log(
            "mid",
            "example-model",
            "code",
            12_345,
            Duration::from_millis(1_234),
        )
        .unwrap();

        assert_eq!(
            out.sink().stdout(),
            format!("# before\n\n{report}\n"),
            "the existing blank-line separator and one newline are unchanged"
        );
        assert_eq!(out.sink().stderr(), "", "the report is stdout output");
        assert_eq!(out.last(), Some(OutputKind::Log));
        assert_eq!(
            out.sink().ops,
            [
                Op::Stdout("# before\n".into()),
                Op::FlushStdout,
                Op::Stdout(format!("\n{report}\n")),
                Op::FlushStdout,
            ],
            "the report is written and flushed exactly like a log"
        );
    }

    /// Contract: a write or flush failure surfaces the existing I/O error and
    /// commits neither the report's output kind nor its spacing transition.
    #[test]
    fn when_model_attempt_is_reported_given_write_or_flush_failure_then_error_and_state_are_preserved()
     {
        let report = "#thinking (1.234s) giga model=review-model phase=review context=12,345 B";

        let mut write_failed = OutputCoordinator::new(RecordingSink::new());
        write_failed.stdout_log("[INFO] existing").unwrap();
        write_failed.sink_mut().fail = true;
        let error = write_failed
            .thinking_log(
                "giga",
                "review-model",
                "review",
                12_345,
                Duration::from_millis(1_234),
            )
            .unwrap_err();
        assert_eq!(error.to_string(), "stdout write failed");
        assert_eq!(write_failed.sink().stdout(), "[INFO] existing\n");
        assert_eq!(write_failed.last(), Some(OutputKind::Log));
        write_failed.sink_mut().fail = false;
        write_failed.stdout_log("[INFO] after").unwrap();
        assert_eq!(
            write_failed.sink().stdout(),
            "[INFO] existing\n[INFO] after\n",
            "a failed report write must not leave a review separator behind"
        );

        let mut flush_failed = OutputCoordinator::new(RecordingSink::new());
        flush_failed.stdout_log("[INFO] existing").unwrap();
        flush_failed.sink_mut().fail_flush = true;
        let error = flush_failed
            .thinking_log(
                "giga",
                "review-model",
                "review",
                12_345,
                Duration::from_millis(1_234),
            )
            .unwrap_err();
        assert_eq!(error.to_string(), "stdout flush failed");
        assert_eq!(
            flush_failed.sink().stdout(),
            format!("[INFO] existing\n{report}\n"),
            "the failed flush happened after the bytes were handed to the sink"
        );
        assert_eq!(flush_failed.last(), Some(OutputKind::Log));
        flush_failed.sink_mut().fail_flush = false;
        flush_failed.stdout_log("[INFO] after").unwrap();
        assert_eq!(
            flush_failed.sink().stdout(),
            format!("[INFO] existing\n{report}\n[INFO] after\n"),
            "a failed report flush must not commit the review transition"
        );

        // A prior successful kind distinct from Log makes the withheld report
        // kind observable: with a Log predecessor the report's own kind is
        // indistinguishable from the kind already committed. Both failures must
        // leave the distinct kind in place and keep the generic separator
        // driven by that kind.
        let mut write_failed_after_call = OutputCoordinator::new(RecordingSink::new());
        write_failed_after_call.tool_call("$ existing").unwrap();
        write_failed_after_call.sink_mut().fail = true;
        let error = write_failed_after_call
            .thinking_log(
                "giga",
                "review-model",
                "review",
                12_345,
                Duration::from_millis(1_234),
            )
            .unwrap_err();
        assert_eq!(error.to_string(), "stdout write failed");
        assert_eq!(
            write_failed_after_call.sink().stdout(),
            "$ existing\n",
            "a failed report write must emit no report bytes"
        );
        assert_eq!(
            write_failed_after_call.last(),
            Some(OutputKind::ToolCall),
            "a failed report write must preserve the prior output kind"
        );
        write_failed_after_call.sink_mut().fail = false;
        write_failed_after_call.stdout_log("[INFO] after").unwrap();
        assert_eq!(
            write_failed_after_call.sink().stdout(),
            "$ existing\n\n[INFO] after\n",
            "the preserved prior kind still drives the generic separator"
        );

        let mut flush_failed_after_call = OutputCoordinator::new(RecordingSink::new());
        flush_failed_after_call.tool_call("$ existing").unwrap();
        flush_failed_after_call.sink_mut().fail_flush = true;
        let error = flush_failed_after_call
            .thinking_log(
                "giga",
                "review-model",
                "review",
                12_345,
                Duration::from_millis(1_234),
            )
            .unwrap_err();
        assert_eq!(error.to_string(), "stdout flush failed");
        assert_eq!(
            flush_failed_after_call.sink().stdout(),
            format!("$ existing\n{report}\n"),
            "the failed flush happened after the adjacent report bytes reached the sink"
        );
        assert_eq!(
            flush_failed_after_call.last(),
            Some(OutputKind::ToolCall),
            "a failed report flush must preserve the prior output kind"
        );
        flush_failed_after_call.sink_mut().fail_flush = false;
        flush_failed_after_call.stdout_log("[INFO] after").unwrap();
        assert_eq!(
            flush_failed_after_call.sink().stdout(),
            format!("$ existing\n{report}\n\n[INFO] after\n"),
            "the preserved prior kind still drives the generic separator"
        );
    }

    /// Regression: an INFO, WARN or ERROR log keeps its existing marker-only
    /// colour and never acquires the report/tool-call cyan.
    #[test]
    fn when_severity_logs_are_written_given_terminal_stdout_then_only_existing_markers_are_colored()
    {
        let mut out = OutputCoordinator::new(RecordingSink::new().stdout_terminal(true));
        out.stdout_log("[INFO] ready").unwrap();
        out.stdout_log("[WARN] slow").unwrap();
        out.stdout_log("[ERROR] boom").unwrap();

        assert_eq!(
            out.sink().stdout(),
            format!(
                "{INFO_FG}[INFO]{STYLE_END} ready\n\
                 {WARN_FG}[WARN]{STYLE_END} slow\n\
                 {ERROR_FG}[ERROR]{STYLE_END} boom\n"
            )
        );
        assert_eq!(out.last(), Some(OutputKind::Log));
        assert!(
            !out.sink().stdout().contains(CYAN),
            "severity rendering is independent of report rendering"
        );
    }

    /// Regression: ordinary untagged logs remain byte-for-byte uncoloured on a
    /// terminal stdout.
    #[test]
    fn when_ordinary_logs_are_written_given_terminal_stdout_then_remain_uncolored() {
        let mut out = OutputCoordinator::new(RecordingSink::new().stdout_terminal(true));
        out.stdout_log("plain message").unwrap();
        out.stdout_log("[DEBUG] verbose").unwrap();
        out.stdout_log("multi\nline").unwrap();

        assert_eq!(
            out.sink().stdout(),
            "plain message\n[DEBUG] verbose\nmulti\nline\n"
        );
        assert_eq!(out.last(), Some(OutputKind::Log));
        assert!(
            !out.sink().stdout().contains('\u{1b}'),
            "ordinary logs do not acquire any colour"
        );
    }

    /// Regression: the established review and summary spacing sequence is
    /// unchanged around real reports and their call lines.
    #[test]
    fn when_reports_follow_existing_transcript_output_given_review_and_summary_phases_then_spacing_is_unchanged()
     {
        let mut out = OutputCoordinator::new(RecordingSink::new());
        out.thinking_log(
            "giga",
            "review-model",
            "review",
            12_345,
            Duration::from_millis(1_234),
        )
        .unwrap();
        out.stdout_log("[INFO] review round 1 passed").unwrap();
        out.thinking_log(
            "smoll",
            "summary-model",
            "summary",
            23_456,
            Duration::from_millis(3_000),
        )
        .unwrap();
        out.tool_call("[1/100] (0.250s) $ js: return tools.diff();")
            .unwrap();

        assert_eq!(
            out.sink().stdout(),
            concat!(
                "#thinking (1.234s) giga model=review-model phase=review context=12,345 B\n",
                "\n",
                "[INFO] review round 1 passed\n",
                "\n",
                "#thinking (3.000s) smoll model=summary-model phase=summary context=23,456 B\n",
                "[1/100] (0.250s) $ js: return tools.diff();\n",
            )
        );
        assert_eq!(out.sink().stderr(), "");
        assert_eq!(out.last(), Some(OutputKind::ToolCall));
    }

    /// Empty/zero boundary: zero elapsed time and zero context bytes keep
    /// their exact payload on both output contracts, and a run that submits no
    /// report writes no bytes, separator, colour or output-kind transition.
    #[test]
    fn when_report_boundaries_are_exercised_given_zero_values_then_zero_duration_and_context_are_preserved()
     {
        for terminal in [false, true] {
            let mut out = OutputCoordinator::new(RecordingSink::new().stdout_terminal(terminal));
            out.thinking_log("mid", "zero-model", "code", 0, Duration::ZERO)
                .unwrap();

            assert_eq!(out.sink().stdout(), report_bytes(ZERO_REPORT, terminal));
            assert_eq!(out.last(), Some(OutputKind::Log));
        }

        // A path that stops before a model attempt is reported never reaches
        // the coordinator at all.
        let idle = OutputCoordinator::new(RecordingSink::new().stdout_terminal(true));
        assert_eq!(idle.sink().ops, [], "no report submission writes nothing");
        assert_eq!(idle.sink().stdout(), "", "no separator is emitted");
        assert_eq!(idle.sink().stderr(), "");
        assert_eq!(idle.last(), None, "no output kind is committed");
        assert!(
            !idle.sink().stdout().contains(CYAN),
            "no colour is emitted without a report"
        );
    }

    /// Minimum valid boundary: one millisecond and one byte are preserved and
    /// receive the same terminal/redirected treatment as any other report.
    #[test]
    fn when_report_boundaries_are_exercised_given_one_millisecond_and_one_byte_then_minimum_payload_is_preserved()
     {
        for terminal in [false, true] {
            let mut out = OutputCoordinator::new(RecordingSink::new().stdout_terminal(terminal));
            out.thinking_log("giga", "min-model", "review", 1, Duration::from_millis(1))
                .unwrap();

            assert_eq!(out.sink().stdout(), report_bytes(MINIMUM_REPORT, terminal));
            assert_eq!(out.last(), Some(OutputKind::Log));
        }
    }

    /// Maximum limit: the complete largest duration and grouped byte count are
    /// preserved without truncation, and terminal styling closes after the
    /// whole payload.
    #[test]
    fn when_report_boundaries_are_exercised_given_maximum_duration_and_context_then_complete_payload_is_preserved()
     {
        for terminal in [false, true] {
            let mut out = OutputCoordinator::new(RecordingSink::new().stdout_terminal(terminal));
            out.thinking_log("mid", "max-model", "review", usize::MAX, Duration::MAX)
                .unwrap();

            assert_eq!(out.sink().stdout(), report_bytes(MAXIMUM_REPORT, terminal));
            assert_eq!(out.last(), Some(OutputKind::Log));
        }

        let mut terminal = OutputCoordinator::new(RecordingSink::new().stdout_terminal(true));
        terminal
            .thinking_log("mid", "max-model", "review", usize::MAX, Duration::MAX)
            .unwrap();
        assert!(
            terminal
                .sink()
                .stdout()
                .contains(&format!("{MAXIMUM_REPORT}{STYLE_END}\n")),
            "the reset follows the complete maximum payload before the newline"
        );
    }

    /// Malformed/invalid boundary: a noncanonical phase report is coloured as
    /// a report, while an otherwise identical generic log and a severity-tagged
    /// log keep their independent rendering.
    #[test]
    fn when_report_paths_are_distinguished_given_noncanonical_phase_and_thinking_shaped_log_then_only_report_is_colored()
     {
        let noncanonical = "#thinking (0.001s) mid model=example-model phase=Review context=1 B";
        let lookalike = "#thinking (1.234s) mid model=example-model phase=code context=12,345 B";
        let tagged =
            "[INFO] #thinking (1.234s) mid model=example-model phase=code context=12,345 B";

        let mut report = OutputCoordinator::new(RecordingSink::new().stdout_terminal(true));
        report
            .thinking_log(
                "mid",
                "example-model",
                "Review",
                1,
                Duration::from_millis(1),
            )
            .unwrap();

        let mut generic = OutputCoordinator::new(RecordingSink::new().stdout_terminal(true));
        generic.stdout_log(lookalike).unwrap();

        let mut severity = OutputCoordinator::new(RecordingSink::new().stdout_terminal(true));
        severity.stdout_log(tagged).unwrap();

        assert_eq!(report.sink().stdout(), report_bytes(noncanonical, true));
        assert_eq!(generic.sink().stdout(), format!("{lookalike}\n"));
        assert_eq!(
            severity.sink().stdout(),
            format!(
                "{INFO_FG}[INFO]{STYLE_END} #thinking (1.234s) mid model=example-model phase=code context=12,345 B\n"
            )
        );
        assert!(!generic.sink().stdout().contains(CYAN));
        assert!(!severity.sink().stdout().contains(CYAN));
        assert_eq!(report.last(), Some(OutputKind::Log));
        assert_eq!(generic.last(), Some(OutputKind::Log));
        assert_eq!(severity.last(), Some(OutputKind::Log));
    }

    // -- Thinking-report adjacency before a call line -----------------------

    /// Core entry-point behavior: a successfully written semantic report from
    /// every canonical phase, or from a noncanonical phase, is immediately
    /// followed by the next call line with no intervening blank line. Both
    /// payloads keep their own single terminating newline and their order.
    #[test]
    fn when_a_thinking_report_is_followed_by_a_tool_call_given_each_phase_then_writes_adjacent_lines()
     {
        let call = "[1/100] (0.250s) $ cargo test";
        for phase in [
            "plan", "spec", "tickets", "triage", "code", "review", "summary", "custom",
        ] {
            let report =
                format!("#thinking (0.001s) mid model=example-model phase={phase} context=1 B");
            let mut out = OutputCoordinator::new(RecordingSink::new());
            out.thinking_log("mid", "example-model", phase, 1, Duration::from_millis(1))
                .unwrap();
            out.tool_call(call).unwrap();

            assert_eq!(
                out.sink().stdout(),
                format!("{report}\n{call}\n"),
                "phase {phase:?} must place the call directly after the report"
            );
            assert!(
                !out.sink().stdout().contains("\n\n"),
                "phase {phase:?} must have no blank separator at the report/call boundary"
            );
            let stdout = out.sink().stdout();
            assert!(
                stdout.find(&report) < stdout.find(call),
                "phase {phase:?} keeps the report before the call"
            );
            assert_eq!(
                stdout.matches('\n').count(),
                2,
                "phase {phase:?} keeps exactly one newline per payload"
            );
            assert_eq!(out.sink().stderr(), "", "phase {phase:?} is stdout only");
            assert_eq!(out.last(), Some(OutputKind::ToolCall));
        }
    }

    /// Core entry-point behavior: on terminal stdout each whole payload keeps
    /// its cyan wrapper and full reset before its newline, while the (empty)
    /// separator between them stays uncoloured; redirected stdout is plain.
    #[test]
    fn when_a_thinking_report_is_followed_by_a_tool_call_given_terminal_stdout_then_colors_only_payloads()
     {
        let report = "#thinking (1.234s) mid model=example-model phase=code context=12,345 B";
        let call = "[1/100] (0.250s) $ cargo test";

        let mut terminal = OutputCoordinator::new(RecordingSink::new().stdout_terminal(true));
        terminal
            .thinking_log(
                "mid",
                "example-model",
                "code",
                12_345,
                Duration::from_millis(1_234),
            )
            .unwrap();
        terminal.tool_call(call).unwrap();
        let stdout = terminal.sink().stdout();
        assert_eq!(
            stdout,
            format!("{CYAN}{report}{STYLE_END}\n{CYAN}{call}{STYLE_END}\n")
        );
        assert!(
            stdout.contains(&format!("{STYLE_END}\n{CYAN}")),
            "the report's reset and newline sit right before the call's cyan"
        );
        assert!(!stdout.contains("\n\n"), "the separator stays empty");

        let mut redirected = OutputCoordinator::new(RecordingSink::new());
        redirected
            .thinking_log(
                "mid",
                "example-model",
                "code",
                12_345,
                Duration::from_millis(1_234),
            )
            .unwrap();
        redirected.tool_call(call).unwrap();
        assert_eq!(redirected.sink().stdout(), format!("{report}\n{call}\n"));
        assert!(
            !redirected.sink().stdout().contains('\u{1b}'),
            "a redirected transcript stays free of ANSI"
        );
        assert_eq!(redirected.sink().stderr(), "");
    }

    /// Edge-case table: earlier transcript output, including an ordinary
    /// message immediately before the report, does not prevent the one-step
    /// adjacency at the report/call boundary.
    #[test]
    fn when_a_thinking_report_is_followed_by_a_tool_call_given_prior_transcript_output_then_preserves_one_step_adjacency()
     {
        let report = "#thinking (0.001s) mid model=example-model phase=code context=1 B";
        let call = "[1/100] (0.250s) $ cargo test";

        let mut out = OutputCoordinator::new(RecordingSink::new());
        out.narration("# earlier narration").unwrap();
        out.stdout_log("earlier note").unwrap();
        out.thinking_log("mid", "example-model", "code", 1, Duration::from_millis(1))
            .unwrap();
        out.tool_call(call).unwrap();

        assert_eq!(
            out.sink().stdout(),
            format!("# earlier narration\n\nearlier note\n{report}\n{call}\n"),
            "only the report/call boundary becomes adjacent; earlier spacing is unchanged"
        );
        assert_eq!(out.last(), Some(OutputKind::ToolCall));
    }

    /// Edge-case table: zero, the minimum valid report/call, and the maximum
    /// capped call with Duration::MAX and usize::MAX all keep exact bytes.
    #[test]
    fn when_report_and_call_payloads_reach_boundaries_given_zero_minimum_and_maximum_values_then_preserve_exact_bytes()
     {
        let zero_report = "#thinking (0.000s) mid model=zero-model phase=code context=0 B";
        let zero_call = "[1/1] $";
        let minimum_report = "#thinking (0.001s) giga model=min-model phase=review context=1 B";
        let minimum_call = call_line(1, 1, "", None);
        let display = "x".repeat(MAX_CALL_LINE * 4);
        let maximum_call = call_line(100, 100, &display, Some(Duration::from_millis(250)));

        let cases = [
            (
                "mid",
                "zero-model",
                "code",
                0usize,
                Duration::ZERO,
                zero_report,
                zero_call,
            ),
            (
                "giga",
                "min-model",
                "review",
                1usize,
                Duration::from_millis(1),
                minimum_report,
                minimum_call.as_str(),
            ),
            (
                "mid",
                "max-model",
                "review",
                usize::MAX,
                Duration::MAX,
                MAXIMUM_REPORT,
                maximum_call.as_str(),
            ),
        ];

        for (provider, model, phase, context_bytes, elapsed, report, call) in cases {
            let mut out = OutputCoordinator::new(RecordingSink::new());
            out.thinking_log(provider, model, phase, context_bytes, elapsed)
                .unwrap();
            out.tool_call(call).unwrap();

            assert_eq!(
                out.sink().stdout(),
                format!("{report}\n{call}\n"),
                "boundary report {report:?} and call {call:?} keep exact bytes"
            );
            assert_eq!(
                out.sink().stdout().matches('\n').count(),
                2,
                "boundary report {report:?} keeps one newline per payload"
            );
            assert!(!out.sink().stdout().contains("\n\n"));
            assert_eq!(out.sink().stderr(), "");
            assert_eq!(out.last(), Some(OutputKind::ToolCall));
        }
    }

    /// Edge-case table: ordinary logs whose text looks like a report keep the
    /// generic separators both after a call and before a following call.
    #[test]
    fn when_report_shaped_logs_are_submitted_given_tool_calls_then_keep_generic_spacing() {
        let call = "[1/100] (0.250s) $ cargo test";
        let next_call = "[2/100] (0.100s) $ js: return tools.diff();";
        let lookalike = "#thinking (1.234s) mid model=example-model phase=code context=12,345 B";
        let malformed = "#thinking (broken phase=code";

        for payload in [lookalike, malformed] {
            let mut out = OutputCoordinator::new(RecordingSink::new());
            out.tool_call(call).unwrap();
            out.stdout_log(payload).unwrap();
            assert_eq!(
                out.sink().stdout(),
                format!("{call}\n\n{payload}\n"),
                "a report-shaped ordinary log after a call keeps the generic blank line"
            );
            out.tool_call(next_call).unwrap();
            assert_eq!(
                out.sink().stdout(),
                format!("{call}\n\n{payload}\n\n{next_call}\n"),
                "a report-shaped ordinary log before a call keeps the kind-change blank line"
            );
            assert_eq!(out.last(), Some(OutputKind::ToolCall));
        }

        let mut out = OutputCoordinator::new(RecordingSink::new());
        out.stdout_log(lookalike).unwrap();
        out.tool_call(call).unwrap();
        assert_eq!(
            out.sink().stdout(),
            format!("{lookalike}\n\n{call}\n"),
            "an ordinary report-shaped log cannot make the following call adjacent"
        );
    }

    /// Contract: the original I/O error surfaces and no failed report or call
    /// commits a semantic transition, so later spacing follows the last
    /// successfully written output.
    #[test]
    fn when_a_report_or_call_write_fails_given_a_sink_error_then_preserves_the_last_successful_output()
     {
        let report = "#thinking (1.234s) mid model=example-model phase=code context=12,345 B";
        let call = "[1/100] (0.250s) $ cargo test";

        // A failed report write leaves narration as the last successful
        // output, so the following call keeps the generic blank separator.
        let mut write_failed = OutputCoordinator::new(RecordingSink::new());
        write_failed.narration("# existing").unwrap();
        write_failed.sink_mut().fail = true;
        let error = write_failed
            .thinking_log(
                "mid",
                "example-model",
                "code",
                12_345,
                Duration::from_millis(1_234),
            )
            .unwrap_err();
        assert_eq!(error.to_string(), "stdout write failed");
        assert_eq!(write_failed.sink().stdout(), "# existing\n");
        assert_eq!(write_failed.last(), Some(OutputKind::Narration));
        write_failed.sink_mut().fail = false;
        write_failed.tool_call(call).unwrap();
        assert_eq!(
            write_failed.sink().stdout(),
            format!("# existing\n\n{call}\n"),
            "a failed report must not make the following call adjacent"
        );

        // A failed report flush also commits no semantic predecessor even
        // though the bytes reached the sink.
        let mut flush_failed = OutputCoordinator::new(RecordingSink::new());
        flush_failed.narration("# existing").unwrap();
        flush_failed.sink_mut().fail_flush = true;
        let error = flush_failed
            .thinking_log(
                "mid",
                "example-model",
                "code",
                12_345,
                Duration::from_millis(1_234),
            )
            .unwrap_err();
        assert_eq!(error.to_string(), "stdout flush failed");
        assert_eq!(
            flush_failed.sink().stdout(),
            format!("# existing\n\n{report}\n")
        );
        assert_eq!(flush_failed.last(), Some(OutputKind::Narration));
        flush_failed.sink_mut().fail_flush = false;
        flush_failed.tool_call(call).unwrap();
        assert_eq!(
            flush_failed.sink().stdout(),
            format!("# existing\n\n{report}\n\n{call}\n"),
            "a failed report flush must not commit adjacency"
        );

        // A failed call leaves the successful report as the last committed
        // output, so the next call is still adjacent.
        let mut call_failed = OutputCoordinator::new(RecordingSink::new());
        call_failed
            .thinking_log(
                "mid",
                "example-model",
                "code",
                12_345,
                Duration::from_millis(1_234),
            )
            .unwrap();
        call_failed.sink_mut().fail = true;
        let error = call_failed.tool_call(call).unwrap_err();
        assert_eq!(error.to_string(), "stdout write failed");
        assert_eq!(call_failed.sink().stdout(), format!("{report}\n"));
        assert_eq!(call_failed.last(), Some(OutputKind::Log));
        call_failed.sink_mut().fail = false;
        call_failed.tool_call(call).unwrap();
        assert_eq!(
            call_failed.sink().stdout(),
            format!("{report}\n{call}\n"),
            "a failed call commits no transition and the report stays adjacent"
        );
    }

    /// Empty/zero boundary: an omitted transcript writes no bytes, separator,
    /// colour, or output-kind transition; omission is not a report.
    #[test]
    fn when_a_transcript_has_no_messages_given_an_empty_output_stream_then_writes_no_bytes() {
        let idle = OutputCoordinator::new(RecordingSink::new().stdout_terminal(true));
        assert_eq!(idle.sink().ops, [], "no message reaches the sink");
        assert_eq!(
            idle.sink().stdout(),
            "",
            "no separator or colour is emitted"
        );
        assert_eq!(idle.sink().stderr(), "");
        assert_eq!(idle.last(), None, "no output kind is committed");
    }

    /// Regression: an interrupted summary pair does not re-arm, but the later
    /// summary report is still a semantic report, so its call is adjacent.
    #[test]
    fn when_a_summary_report_follows_an_interruption_given_a_tool_call_then_does_not_rearm_the_summary_pair()
     {
        let first = "#thinking (3.000s) smoll model=summary-model phase=summary context=23,456 B";
        let second = "#thinking (4.000s) smoll model=summary-model phase=summary context=23,457 B";
        let first_call = "[1/100] (0.250s) $ js: return tools.diff();";
        let second_call = "[2/100] (0.100s) $ js: return tools.diff();";

        let mut out = OutputCoordinator::new(RecordingSink::new());
        out.thinking_log(
            "smoll",
            "summary-model",
            "summary",
            23_456,
            Duration::from_millis(3_000),
        )
        .unwrap();
        out.tool_call(first_call).unwrap();
        out.narration("# interruption").unwrap();
        out.thinking_log(
            "smoll",
            "summary-model",
            "summary",
            23_457,
            Duration::from_millis(4_000),
        )
        .unwrap();
        out.tool_call(second_call).unwrap();

        assert_eq!(
            out.sink().stdout(),
            format!("{first}\n{first_call}\n\n# interruption\n\n{second}\n{second_call}\n"),
            "the later report is adjacent to its call without restoring the pair exception"
        );
        assert!(!out.sink().stdout().contains(&format!("{second}\n\n")));
        assert_eq!(out.sink().stderr(), "");
        assert_eq!(out.last(), Some(OutputKind::ToolCall));
    }

    /// Regression: the reverse direction remains adjacent for every canonical
    /// phase and for a noncanonical phase.
    #[test]
    fn when_a_tool_call_is_followed_by_a_thinking_report_given_any_phase_then_remains_adjacent() {
        let call = "[1/100] (0.250s) $ cargo test";
        for phase in [
            "plan", "spec", "tickets", "triage", "code", "review", "summary", "custom",
        ] {
            let report =
                format!("#thinking (0.001s) mid model=example-model phase={phase} context=1 B");
            let mut out = OutputCoordinator::new(RecordingSink::new());
            out.tool_call(call).unwrap();
            out.thinking_log("mid", "example-model", phase, 1, Duration::from_millis(1))
                .unwrap();

            assert_eq!(
                out.sink().stdout(),
                format!("{call}\n{report}\n"),
                "phase {phase:?} must stay adjacent after a call line"
            );
            assert!(!out.sink().stdout().contains("\n\n"));
            assert_eq!(out.last(), Some(OutputKind::Log));
        }
    }

    /// Regression: outside the semantic report/call boundary and the
    /// documented review/summary transitions, generic spacing is unchanged --
    /// first message, same kind, kind change, stderr adjacency and flush order.
    #[test]
    fn when_stdout_messages_change_kind_given_non_special_transitions_then_keeps_generic_spacing() {
        let first_call = "[1/100] (0.250s) $ cargo test";
        let second_call = "[2/100] (0.100s) $ ls";

        let mut out = OutputCoordinator::new(RecordingSink::new());
        out.narration("# first").unwrap();
        assert_eq!(
            out.sink().stdout(),
            "# first\n",
            "the first message has no leading separator"
        );
        out.narration("# second").unwrap();
        assert_eq!(
            out.sink().stdout(),
            "# first\n# second\n",
            "consecutive messages of the same kind have no separator"
        );
        out.stdout_log("[INFO] ready").unwrap();
        assert_eq!(
            out.sink().stdout(),
            "# first\n# second\n\n[INFO] ready\n",
            "a change of kind has exactly one blank separator line"
        );
        out.tool_call(first_call).unwrap();
        assert_eq!(
            out.sink().stdout(),
            format!("# first\n# second\n\n[INFO] ready\n\n{first_call}\n")
        );
        out.tool_call(second_call).unwrap();
        assert_eq!(
            out.sink().stdout(),
            format!("# first\n# second\n\n[INFO] ready\n\n{first_call}\n{second_call}\n"),
            "consecutive calls stay adjacent"
        );

        out.stderr_log("[WARN] diagnostic").unwrap();
        assert_eq!(out.sink().stderr(), "\n[WARN] diagnostic\n");
        let ops = &out.sink().ops;
        assert_eq!(ops.get(ops.len() - 3), Some(&Op::FlushStdout));
        assert!(matches!(ops.get(ops.len() - 2), Some(Op::Stderr(_))));
        assert_eq!(ops.last(), Some(&Op::FlushStderr));
        assert_eq!(out.last(), Some(OutputKind::Log));
    }

    /// Empty/zero boundary: zero duration and zero context preserve their
    /// exact metadata and each payload's single newline.
    #[test]
    fn when_report_and_call_payloads_reach_boundaries_given_zero_values_then_preserve_zero_metadata_and_newlines()
     {
        let report = "#thinking (0.000s) mid model=zero-model phase=code context=0 B";
        let call = "[1/1] $";
        let mut out = OutputCoordinator::new(RecordingSink::new());
        out.thinking_log("mid", "zero-model", "code", 0, Duration::ZERO)
            .unwrap();
        out.tool_call(call).unwrap();

        assert_eq!(out.sink().stdout(), format!("{report}\n{call}\n"));
        assert!(out.sink().stdout().contains("(0.000s)"));
        assert!(out.sink().stdout().contains("context=0 B"));
        assert_eq!(out.sink().stdout().matches('\n').count(), 2);
        assert_eq!(out.last(), Some(OutputKind::ToolCall));
    }

    /// Minimum valid boundary: the shortest production call and the one
    /// millisecond/one byte report keep exact framing.
    #[test]
    fn when_report_and_call_payloads_reach_boundaries_given_the_shortest_call_and_minimum_report_then_preserves_exact_framing()
     {
        let call = call_line(1, 1, "", None);
        assert_eq!(call, "[1/1] $", "the shortest call line is exactly this");
        let report = "#thinking (0.001s) giga model=min-model phase=review context=1 B";
        let mut out = OutputCoordinator::new(RecordingSink::new());
        out.thinking_log("giga", "min-model", "review", 1, Duration::from_millis(1))
            .unwrap();
        out.tool_call(&call).unwrap();

        assert_eq!(out.sink().stdout(), format!("{report}\n{call}\n"));
        assert!(out.sink().stdout().contains("(0.001s)"));
        assert!(out.sink().stdout().contains("context=1 B"));
        assert_eq!(out.last(), Some(OutputKind::ToolCall));
    }

    /// Maximum limit: the capped call line and the complete maximum report
    /// metadata keep their exact framing and adjacency.
    #[test]
    fn when_report_and_call_payloads_reach_boundaries_given_maximum_call_and_report_values_then_preserves_cap_metadata_and_framing()
     {
        let display = "x".repeat(MAX_CALL_LINE * 4);
        let call = call_line(100, 100, &display, Some(Duration::from_millis(250)));
        assert_eq!(
            call.chars().count(),
            MAX_CALL_LINE,
            "the call line is capped at the production limit"
        );

        let mut out = OutputCoordinator::new(RecordingSink::new());
        out.thinking_log("mid", "max-model", "review", usize::MAX, Duration::MAX)
            .unwrap();
        out.tool_call(&call).unwrap();

        assert_eq!(
            out.sink().stdout(),
            format!("{MAXIMUM_REPORT}\n{call}\n"),
            "the complete maximum report and capped call are adjacent"
        );
        assert!(out.sink().stdout().contains("(18446744073709551615.999s)"));
        assert!(
            out.sink()
                .stdout()
                .contains("context=18,446,744,073,709,551,615 B")
        );
        assert_eq!(out.last(), Some(OutputKind::ToolCall));
    }

    /// Malformed/invalid boundary: a report-shaped lookalike and a malformed
    /// report-shaped line remain ordinary logs around calls.
    #[test]
    fn when_report_shaped_logs_are_submitted_given_malformed_and_lookalike_text_then_remain_ordinary()
     {
        let call = "[1/100] (0.250s) $ cargo test";
        let next_call = "[2/100] (0.100s) $ ls";
        for payload in [
            "#thinking (1.234s) mid model=example-model phase=code context=12,345 B",
            "#thinking (broken phase=review",
        ] {
            let mut out = OutputCoordinator::new(RecordingSink::new());
            out.tool_call(call).unwrap();
            out.stdout_log(payload).unwrap();
            out.tool_call(next_call).unwrap();

            assert_eq!(
                out.sink().stdout(),
                format!("{call}\n\n{payload}\n\n{next_call}\n"),
                "the ordinary log keeps the generic separators: {payload:?}"
            );
            assert_eq!(out.last(), Some(OutputKind::ToolCall));
        }
    }

    /// Malformed/invalid boundary: an unrecognized phase submitted through the
    /// semantic reporting route remains eligible for adjacency.
    #[test]
    fn when_a_semantic_report_uses_an_unknown_phase_given_a_tool_call_then_remains_adjacent() {
        let report = "#thinking (0.001s) mid model=example-model phase=mystery context=1 B";
        let call = "[1/100] (0.250s) $ cargo test";
        let mut out = OutputCoordinator::new(RecordingSink::new());
        out.thinking_log(
            "mid",
            "example-model",
            "mystery",
            1,
            Duration::from_millis(1),
        )
        .unwrap();
        out.tool_call(call).unwrap();

        assert_eq!(
            out.sink().stdout(),
            format!("{report}\n{call}\n"),
            "phase recognition is not a prerequisite for report/call adjacency"
        );
        assert!(!out.sink().stdout().contains("\n\n"));
        assert_eq!(out.last(), Some(OutputKind::ToolCall));
    }
}
