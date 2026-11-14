//! Elapsed-time measurement for one operation, on one shared path.
//!
//! The agent loop's model-issued command tools -- and, later, the pipeline's own
//! phases -- all want to say how long something took. Measuring and reporting
//! are deliberately two halves of one small seam here: [`measure`] times a
//! closure and hands a label and the duration to a [`Reporter`], while
//! everything about the destination, the framing and the exact text stays with
//! the reporter. Production's reporter lives in [`crate::status`], beside the
//! other output conventions; a unit test supplies its own, so the measurement is
//! assertable without a stream.

use std::time::{Duration, Instant};

/// A monotonic source of elapsed durations.
///
/// The loop needs two readings of one monotonic timeline to measure its own
/// body. Production supplies MonotonicClock; a unit test supplies a local
/// stand-in so the measured interval is controlled without sleeping. The trait
/// stays private to the crate: only the loop's own timing seam reads it.
pub(crate) trait Clock {
    /// The current point on the monotonic timeline.
    fn now(&self) -> Duration;
}

/// The production clock: an instant captured when the run starts, with every
/// reading measured from it. Instant is monotonic, so wall-clock changes
/// cannot move the measured interval backwards.
pub(crate) struct MonotonicClock {
    origin: Instant,
}

impl MonotonicClock {
    /// Start a new monotonic timeline at the current instant.
    pub(crate) fn new() -> MonotonicClock {
        MonotonicClock {
            origin: Instant::now(),
        }
    }
}

impl Clock for MonotonicClock {
    fn now(&self) -> Duration {
        self.origin.elapsed()
    }
}

/// Where the elapsed duration of one finished operation is reported.
///
/// The seam between measuring an operation and showing its duration: the
/// wrapper knows nothing about labels beyond passing one on, and nothing at all
/// about streams or formatting. Production implements this once, over the shared
/// output coordinator ([`crate::status`]), and a unit test implements it with a
/// recorder.
pub trait Reporter {
    /// Report one finished `label` operation that took `elapsed`.
    fn report(&mut self, label: &str, elapsed: Duration);
}

/// Time `operation` and return both its value and the elapsed duration,
/// without reporting anything.
///
/// This is the measurement half of [`measure`] on its own: a caller that wants
/// to place the duration somewhere other than an elapsed line -- on a tool
/// call's permanent echo, for example -- gets the exact interval without a
/// report being written behind its back. The clock is read immediately before
/// the closure is entered and immediately after it returns, so what is measured
/// is the operation alone: not the request, the parse or the step that led to
/// it, and not the presentation work the caller does afterwards.
///
/// The value comes back exactly as the closure produced it: a success, an error,
/// an absent value, anything at all. Timing is additive, so it observes and
/// never inspects, rewrites or swallows what the operation returned.
pub fn measured<T, F>(operation: F) -> (T, Duration)
where
    F: FnOnce() -> T,
{
    let started = Instant::now();
    let value = operation();
    (value, started.elapsed())
}

/// Time `operation`, report its elapsed duration once, and return the
/// operation's value unchanged.
///
/// The measurement is [`measured`]'s, so the interval and the value's
/// transparency are exactly the same. The duration is taken before the reporter
/// runs, so the report itself -- a write, however slow -- is outside the measured
/// interval, and the report follows the operation.
///
/// The value comes back exactly as the closure produced it: a success, an error,
/// an absent value, anything at all. Timing is additive, so it observes and
/// reports, and never inspects, rewrites or swallows what the operation
/// returned.
pub fn measure<T, R, F>(reporter: &mut R, label: &str, operation: F) -> T
where
    R: Reporter + ?Sized,
    F: FnOnce() -> T,
{
    let (value, elapsed) = measured(operation);
    reporter.report(label, elapsed);
    value
}

/// One measured queue round's whole-millisecond timing attribution, in the
/// shape a metrics artifact serializes.
///
/// Every field is a non-negative whole millisecond count: a duration's
/// fractional millisecond is floored away and a duration whose millisecond
/// count cannot fit in a `u64` saturates at `u64::MAX` rather than wrapping.
/// The four category fields are the accumulator's own sums and the residual is
/// computed from the unrounded contributions, so flooring one category can
/// never move the unaccounted time into another.
///
/// The type is crate-internal and serializable, with every field defaulting to
/// zero when a payload omits it, so a round that reported no timing still
/// decodes and a report written before a category existed stays readable. It
/// reports nothing by itself: the measured queue round owns where the values go.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub(crate) struct QueueTimingBreakdown {
    /// The measured whole-round interval.
    pub(crate) total_elapsed_ms: u64,
    /// The part attributed to completed provider attempts.
    pub(crate) model_attempt_ms: u64,
    /// The part attributed to executed command tools.
    pub(crate) executed_tool_ms: u64,
    /// The part attributed to verification gates.
    pub(crate) verification_gate_ms: u64,
    /// The part of the round attributed to no measured contributor.
    pub(crate) residual_overhead_ms: u64,
}

/// Ephemeral accounting of the time one queue round already spent inside a
/// ticket, split into the categories the queue will report separately.
///
/// It is deliberately not a timer and not a store: every duration handed to a
/// `record_*` method was already measured by the component that owns the
/// operation -- one provider attempt, one executed command, one verification
/// gate -- so this type only adds the contributions up. Each category is an
/// independent `Duration` sum, and both accumulation and the residual are
/// saturating: a contributor can never wrap, and a round shorter than the time
/// attributed to its parts yields a residual of zero rather than an underflow.
///
/// The accumulator is per-queue and per-round: nothing here persists, and
/// nothing reports by itself. Later queue accounting owns where the sums and
/// the residual are rendered.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct QueueContributions {
    model_attempts: Duration,
    executed_tools: Duration,
    verification_gates: Duration,
}

#[allow(dead_code)] // The later queue-accounting ticket reads these sums.
impl QueueContributions {
    /// No contributions yet.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Add one completed provider attempt's measured duration.
    pub(crate) fn record_model_attempt(&mut self, elapsed: Duration) {
        self.model_attempts = self.model_attempts.saturating_add(elapsed);
    }

    /// Add one executed command's already-measured duration.
    pub(crate) fn record_executed_tool(&mut self, elapsed: Duration) {
        self.executed_tools = self.executed_tools.saturating_add(elapsed);
    }

    /// Add one verification gate's measured duration.
    pub(crate) fn record_verification_gate(&mut self, elapsed: Duration) {
        self.verification_gates = self.verification_gates.saturating_add(elapsed);
    }

    /// The total duration attributed to model attempts.
    pub(crate) fn model_attempts(&self) -> Duration {
        self.model_attempts
    }

    /// The total duration attributed to executed commands.
    pub(crate) fn executed_tools(&self) -> Duration {
        self.executed_tools
    }

    /// The total duration attributed to verification gates.
    pub(crate) fn verification_gates(&self) -> Duration {
        self.verification_gates
    }

    /// Every category's contribution, added together.
    pub(crate) fn total(&self) -> Duration {
        self.model_attempts
            .saturating_add(self.executed_tools)
            .saturating_add(self.verification_gates)
    }

    /// The part of `elapsed` not attributed to any contribution, saturating at
    /// zero so a round shorter than its attributed parts reports no negative
    /// residual.
    pub(crate) fn residual(&self, elapsed: Duration) -> Duration {
        elapsed.saturating_sub(self.total())
    }

    /// Attribute one measured round to its contributors and its residual as
    /// whole milliseconds.
    ///
    /// `total_elapsed` is the interval the queue round actually measured, and
    /// every category is this accumulator's own sum. The residual is
    /// `total_elapsed` minus the unrounded contributions, computed before any
    /// value is floored, so the parts and the residual can never disagree by a
    /// carried millisecond. A round shorter than its attributed parts reports a
    /// zero residual, and a count too large for a `u64` saturates.
    pub(crate) fn breakdown(&self, total_elapsed: Duration) -> QueueTimingBreakdown {
        QueueTimingBreakdown {
            total_elapsed_ms: whole_milliseconds(total_elapsed),
            model_attempt_ms: whole_milliseconds(self.model_attempts),
            executed_tool_ms: whole_milliseconds(self.executed_tools),
            verification_gate_ms: whole_milliseconds(self.verification_gates),
            residual_overhead_ms: whole_milliseconds(self.residual(total_elapsed)),
        }
    }
}

/// One duration as a non-negative whole millisecond count: the fractional
/// millisecond is floored away and a value beyond `u64::MAX` saturates.
fn whole_milliseconds(elapsed: Duration) -> u64 {
    u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    /// A reporter that records every `(label, elapsed)` pair it is handed, in
    /// order: the recording side of the seam. It writes no bytes and touches no
    /// stream, so a unit test can assert what the wrapper reported without a
    /// terminal, a captured stream or a child process.
    #[derive(Default)]
    struct Recorder {
        reports: Mutex<Vec<(String, Duration)>>,
    }

    impl Recorder {
        fn reports(&self) -> Vec<(String, Duration)> {
            self.reports.lock().unwrap().clone()
        }

        fn labels(&self) -> Vec<String> {
            self.reports().into_iter().map(|(label, _)| label).collect()
        }
    }

    impl Reporter for Recorder {
        fn report(&mut self, label: &str, elapsed: Duration) {
            self.reports
                .lock()
                .unwrap()
                .push((label.to_string(), elapsed));
        }
    }

    /// A reporter that appends one `report:<label>` entry to a shared journal
    /// after the operation appended its own entry: the double the ordering test
    /// uses, where both sides write to one log so their order is observable.
    struct Journal {
        entries: Arc<Mutex<Vec<String>>>,
    }

    impl Journal {
        fn new(entries: Arc<Mutex<Vec<String>>>) -> Self {
            Self { entries }
        }

        fn entries(&self) -> Vec<String> {
            self.entries.lock().unwrap().clone()
        }
    }

    impl Reporter for Journal {
        fn report(&mut self, label: &str, _elapsed: Duration) {
            self.entries.lock().unwrap().push(format!("report:{label}"));
        }
    }

    /// Core entry-point behavior: the wrapper is transparent. A success comes
    /// back exactly as the closure produced it, and the operation is reported
    /// exactly once.
    #[test]
    fn when_operation_finishes_given_success_then_preserves_result_and_reports_once() {
        let mut reporter = Recorder::default();

        let value = measure(&mut reporter, "bash", || {
            Ok::<String, String>("all green".to_string())
        });

        assert_eq!(
            value,
            Ok("all green".to_string()),
            "the success must not be rewritten"
        );
        assert_eq!(
            reporter.labels(),
            ["bash"],
            "one report, naming the operation"
        );
        assert_eq!(reporter.reports().len(), 1, "the report happens once");
    }

    /// Core entry-point behavior: the error case is the same contract. The error
    /// comes back untouched and the report still happens exactly once.
    #[test]
    fn when_operation_fails_given_error_then_preserves_error_and_reports_once() {
        let mut reporter = Recorder::default();

        let value = measure(&mut reporter, "execute_script", || {
            Err::<String, String>("[ERROR] boom".to_string())
        });

        assert_eq!(
            value,
            Err("[ERROR] boom".to_string()),
            "the error must not be replaced"
        );
        assert_eq!(reporter.labels(), ["execute_script"]);
        assert_eq!(reporter.reports().len(), 1);
    }

    /// Zero/absent value: an incomplete value is a value like any other. It is
    /// handed back as it is, and the operation is still reported once.
    #[test]
    fn when_operation_returns_nothing_given_absent_value_then_preserves_absence_and_reports_once() {
        let mut reporter = Recorder::default();

        let value = measure(&mut reporter, "bash", || None::<String>);

        assert_eq!(value, None, "an absent value must stay absent");
        assert_eq!(reporter.labels(), ["bash"]);
        assert_eq!(reporter.reports().len(), 1);
    }

    /// Contract: reporting is additive and ordered. The report follows the
    /// operation without replacing what it returned, so the journal shows the
    /// operation's own entry first and the report second, and the error that
    /// comes back is byte for byte the closure's.
    #[test]
    fn when_operation_returns_given_error_then_reports_after_operation_without_replacing_error() {
        let entries = Arc::new(Mutex::new(Vec::new()));
        let mut reporter = Journal::new(Arc::clone(&entries));
        let operation_entries = Arc::clone(&entries);

        let value = measure(&mut reporter, "bash", move || {
            operation_entries
                .lock()
                .unwrap()
                .push("operation".to_string());
            Err::<String, String>("[ERROR: exit code 7 for `exit 7`]".to_string())
        });

        assert_eq!(value, Err("[ERROR: exit code 7 for `exit 7`]".to_string()));
        assert_eq!(
            reporter.entries(),
            ["operation", "report:bash"],
            "the operation finishes, then the report lands"
        );
    }

    /// Reporting is per operation: two operations through one reporter produce
    /// one report each, in order, each naming its own operation.
    #[test]
    fn when_multiple_operations_finish_given_separate_calls_then_reports_each_once() {
        let mut reporter = Recorder::default();

        let _ = measure(&mut reporter, "bash", || "first");
        let _ = measure(&mut reporter, "execute_script", || "second");

        assert_eq!(reporter.labels(), ["bash", "execute_script"]);
        assert_eq!(reporter.reports().len(), 2);
    }

    /// Core entry-point behavior for the inline-duration path: the operation's
    /// value comes back unchanged beside a duration that encloses an interval
    /// measured wholly inside it, and no report is written -- the caller
    /// decides where the duration goes.
    #[test]
    fn when_operation_finishes_given_success_then_preserves_result_and_elapsed_duration_without_reporting()
     {
        let ((value, inside), elapsed) = measured(|| {
            let started = Instant::now();
            let value = Ok::<String, String>("all green".to_string());
            (value, started.elapsed())
        });

        assert_eq!(
            value,
            Ok("all green".to_string()),
            "the success must not be rewritten"
        );
        assert!(
            elapsed >= inside,
            "the returned duration must enclose the interval measured inside the operation"
        );
    }

    /// Contract: the error case is unchanged too. The error comes back byte for
    /// byte beside a duration that encloses an interval measured wholly inside
    /// the operation, and nothing is reported behind the caller's back.
    #[test]
    fn when_operation_fails_given_error_then_preserves_error_and_elapsed_duration_without_reporting()
     {
        let ((value, inside), elapsed) = measured(|| {
            let started = Instant::now();
            let value = Err::<String, String>("[ERROR] boom".to_string());
            (value, started.elapsed())
        });

        assert_eq!(value, Err("[ERROR] boom".to_string()));
        assert!(
            elapsed >= inside,
            "the returned duration must enclose the interval measured inside the operation"
        );
    }

    /// Zero/absent value: an absent value is a value like any other, and the
    /// duration is still returned, enclosing an interval measured wholly inside
    /// the operation, without a report.
    #[test]
    fn when_operation_returns_nothing_given_absent_value_then_preserves_absence_and_elapsed_duration_without_reporting()
     {
        let ((value, inside), elapsed) = measured(|| {
            let started = Instant::now();
            let value = None::<String>;
            (value, started.elapsed())
        });

        assert_eq!(value, None);
        assert!(
            elapsed >= inside,
            "the returned duration must enclose the interval measured inside the operation"
        );
    }

    /// Contract: the three categories are independent sums, a later record adds
    /// to its own category only, and the residual is the round's unaccounted
    /// time -- never negative, however much was attributed.
    #[test]
    fn when_queue_timing_is_accounted_given_contributors_exceed_total_then_overhead_is_zero() {
        let mut contributions = QueueContributions::new();
        assert_eq!(contributions.total(), Duration::ZERO);
        assert_eq!(
            contributions.residual(Duration::from_secs(5)),
            Duration::from_secs(5)
        );

        contributions.record_model_attempt(Duration::from_millis(1_500));
        contributions.record_executed_tool(Duration::from_millis(250));
        contributions.record_verification_gate(Duration::from_millis(750));

        assert_eq!(contributions.model_attempts(), Duration::from_millis(1_500));
        assert_eq!(contributions.executed_tools(), Duration::from_millis(250));
        assert_eq!(
            contributions.verification_gates(),
            Duration::from_millis(750)
        );
        assert_eq!(contributions.total(), Duration::from_millis(2_500));

        // A later contribution lands in its own category, leaving the others
        // exactly as they were.
        contributions.record_model_attempt(Duration::from_millis(100));
        assert_eq!(contributions.model_attempts(), Duration::from_millis(1_600));
        assert_eq!(contributions.executed_tools(), Duration::from_millis(250));
        assert_eq!(
            contributions.verification_gates(),
            Duration::from_millis(750)
        );
        assert_eq!(contributions.total(), Duration::from_millis(2_600));

        assert_eq!(
            contributions.residual(Duration::from_secs(3)),
            Duration::from_millis(400)
        );
        assert_eq!(
            contributions.residual(Duration::from_millis(2_600)),
            Duration::ZERO
        );
        assert_eq!(
            contributions.residual(Duration::ZERO),
            Duration::ZERO,
            "an attributed round longer than the round itself reports a zero residual"
        );

        // Accumulation saturates rather than wrapping.
        let mut saturated = QueueContributions::new();
        saturated.record_model_attempt(Duration::MAX);
        saturated.record_model_attempt(Duration::from_secs(1));
        assert_eq!(saturated.model_attempts(), Duration::MAX);
        assert_eq!(saturated.residual(Duration::MAX), Duration::ZERO);
    }

    /// Edge-case table for one round's whole-millisecond conversion: no
    /// contributors and no elapsed time, a round shorter than its attributed
    /// parts, fractional milliseconds, and durations at and past the `u64`
    /// millisecond ceiling. Every case floors its fractional part and saturates
    /// at the ceiling rather than wrapping.
    #[test]
    fn when_round_durations_are_converted_given_zero_fractional_and_oversized_values_then_floors_and_saturates()
     {
        fn contributions(model: Duration, tool: Duration, gate: Duration) -> QueueContributions {
            let mut contributions = QueueContributions::new();
            contributions.record_model_attempt(model);
            contributions.record_executed_tool(tool);
            contributions.record_verification_gate(gate);
            contributions
        }

        let cases: Vec<(&str, QueueContributions, Duration, QueueTimingBreakdown)> = vec![
            (
                "empty contributors and zero elapsed time",
                QueueContributions::new(),
                Duration::ZERO,
                QueueTimingBreakdown {
                    total_elapsed_ms: 0,
                    model_attempt_ms: 0,
                    executed_tool_ms: 0,
                    verification_gate_ms: 0,
                    residual_overhead_ms: 0,
                },
            ),
            (
                "a round shorter than its attributed parts",
                contributions(
                    Duration::from_millis(1_500),
                    Duration::from_millis(250),
                    Duration::from_millis(750),
                ),
                Duration::ZERO,
                QueueTimingBreakdown {
                    total_elapsed_ms: 0,
                    model_attempt_ms: 1_500,
                    executed_tool_ms: 250,
                    verification_gate_ms: 750,
                    residual_overhead_ms: 0,
                },
            ),
            (
                "fractional milliseconds floor their parts and the residual",
                contributions(
                    Duration::from_millis(1_500) + Duration::from_micros(999),
                    Duration::from_micros(250_500),
                    Duration::from_millis(750),
                ),
                Duration::from_micros(3_500_600),
                QueueTimingBreakdown {
                    total_elapsed_ms: 3_500,
                    model_attempt_ms: 1_500,
                    executed_tool_ms: 250,
                    verification_gate_ms: 750,
                    residual_overhead_ms: 999,
                },
            ),
            (
                "durations past the millisecond ceiling saturate",
                contributions(Duration::MAX, Duration::ZERO, Duration::ZERO),
                Duration::MAX,
                QueueTimingBreakdown {
                    total_elapsed_ms: u64::MAX,
                    model_attempt_ms: u64::MAX,
                    executed_tool_ms: 0,
                    verification_gate_ms: 0,
                    residual_overhead_ms: 0,
                },
            ),
        ];

        for (case, queue, elapsed, expected) in cases {
            assert_eq!(queue.breakdown(elapsed), expected, "case: {case}");
        }
    }

    /// Contract: the wire form of the round breakdown tolerates a payload that
    /// names only some fields -- or none at all -- because every field defaults
    /// to zero, and rejects a payload that is not the specified shape.
    #[test]
    fn when_timing_breakdown_is_decoded_given_omitted_fields_or_invalid_payload_then_defaults_or_rejects()
     {
        let empty: QueueTimingBreakdown =
            serde_json::from_str("{}").expect("an empty payload decodes to the defaults");
        assert_eq!(
            empty.total_elapsed_ms, 0,
            "an omitted total defaults to zero"
        );
        assert_eq!(empty.model_attempt_ms, 0);
        assert_eq!(empty.executed_tool_ms, 0);
        assert_eq!(empty.verification_gate_ms, 0);
        assert_eq!(empty.residual_overhead_ms, 0);

        let partial: QueueTimingBreakdown =
            serde_json::from_str(r#"{"model_attempt_ms": 1500, "verification_gate_ms": 750}"#)
                .expect("a payload with omitted fields decodes");
        assert_eq!(partial.total_elapsed_ms, 0);
        assert_eq!(partial.model_attempt_ms, 1_500);
        assert_eq!(partial.executed_tool_ms, 0);
        assert_eq!(partial.verification_gate_ms, 750);
        assert_eq!(partial.residual_overhead_ms, 0);

        let complete: QueueTimingBreakdown = serde_json::from_str(
            r#"{"total_elapsed_ms": 3000, "model_attempt_ms": 1500, "executed_tool_ms": 250, "verification_gate_ms": 750, "residual_overhead_ms": 500}"#,
        )
        .expect("a complete payload decodes");
        assert_eq!(
            complete,
            QueueTimingBreakdown {
                total_elapsed_ms: 3_000,
                model_attempt_ms: 1_500,
                executed_tool_ms: 250,
                verification_gate_ms: 750,
                residual_overhead_ms: 500,
            }
        );

        for (case, payload) in [
            (
                "a negative millisecond value",
                r#"{"total_elapsed_ms": -1}"#,
            ),
            (
                "a stringly typed millisecond value",
                r#"{"total_elapsed_ms": "3000"}"#,
            ),
            (
                "a fractional millisecond value",
                r#"{"residual_overhead_ms": 1.5}"#,
            ),
            ("a null millisecond value", r#"{"total_elapsed_ms": null}"#),
            ("a scalar instead of an object", "3000"),
        ] {
            assert!(
                serde_json::from_str::<QueueTimingBreakdown>(payload).is_err(),
                "the invalid payload must be rejected ({case}): {payload}"
            );
        }
    }
}
