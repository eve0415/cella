//! Terminal progress display using indicatif spinners.
//!
//! Provides [`Progress`] as the central coordinator for all user-facing
//! status output.  When enabled (text mode), operations show animated
//! spinners that resolve to checkmarks on completion.  When disabled
//! (JSON mode, `RUST_LOG` set, non-TTY), every spinner method is a silent
//! no-op.
//!
//! ## Two output axes
//!
//! - **User verbosity** (`--verbose`/`-v`): controls which progress steps
//!   and details are shown. Managed by [`Verbosity`].
//! - **Developer tracing** (`RUST_LOG`): controls `tracing` subscriber
//!   filtering. When `RUST_LOG` is set, spinners are automatically
//!   disabled to avoid corruption.
//!
//! ## Tracing integration
//!
//! [`IndicatifMakeWriter`] routes `tracing` output through
//! [`indicatif::MultiProgress::println`] so structured log lines never
//! corrupt active spinners.

use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use tracing_subscriber::fmt::MakeWriter;

// ── style constants ──────────────────────────────────────────────────

const SPINNER_TICK_CHARS: &str = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ ";
const TICK_INTERVAL: Duration = Duration::from_millis(80);

fn spinner_style() -> ProgressStyle {
    ProgressStyle::with_template("{spinner:.cyan} {msg}")
        .expect("hard-coded template")
        .tick_chars(SPINNER_TICK_CHARS)
}

fn child_spinner_style() -> ProgressStyle {
    ProgressStyle::with_template("  {spinner:.cyan} {msg}")
        .expect("hard-coded template")
        .tick_chars(SPINNER_TICK_CHARS)
}

// ── Verbosity ────────────────────────────────────────────────────────

/// User-facing verbosity level.
///
/// This is independent of `RUST_LOG` tracing. It controls which progress
/// steps are shown, not which tracing events are emitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verbosity {
    /// Default: show phase spinners, lifecycle headers, streaming output.
    Normal,
    /// Expanded: additionally show container names, feature resolution,
    /// UID remapping, SSH/git/credential forwarding, network connections,
    /// individual tool install sub-steps.
    Verbose,
}

impl Verbosity {
    pub fn is_verbose(self) -> bool {
        self == Self::Verbose
    }
}

// ── Progress ─────────────────────────────────────────────────────────

/// Shared progress context threaded through all commands.
#[derive(Clone)]
pub struct Progress {
    inner: Arc<ProgressInner>,
}

struct ProgressInner {
    multi: MultiProgress,
    enabled: bool,
    verbosity: Verbosity,
}

impl Progress {
    /// Create a new progress context.
    ///
    /// When `enabled` is false (JSON mode, `RUST_LOG` set, non-TTY),
    /// spinner methods are silent no-ops and [`MultiProgress`] is hidden.
    pub fn new(enabled: bool, verbosity: Verbosity) -> Self {
        let multi = if enabled {
            MultiProgress::new()
        } else {
            MultiProgress::with_draw_target(indicatif::ProgressDrawTarget::hidden())
        };
        Self {
            inner: Arc::new(ProgressInner {
                multi,
                enabled,
                verbosity,
            }),
        }
    }

    /// Whether spinners are active (text mode, TTY, no `RUST_LOG`).
    pub fn is_enabled(&self) -> bool {
        self.inner.enabled
    }

    /// Current user verbosity level.
    pub fn verbosity(&self) -> Verbosity {
        self.inner.verbosity
    }

    /// Whether verbose mode is active.
    pub fn is_verbose(&self) -> bool {
        self.inner.verbosity.is_verbose()
    }

    /// Access the underlying [`MultiProgress`] for tracing writer setup.
    pub fn multi(&self) -> &MultiProgress {
        &self.inner.multi
    }

    /// Start a spinner for a single operation.
    ///
    /// Top-level steps are permanent: their completion line survives
    /// streaming output from Docker builds and lifecycle commands.
    pub fn step(&self, label: &str) -> Step {
        let bar = self.inner.multi.add(ProgressBar::new_spinner());
        bar.set_style(spinner_style());
        bar.set_message(label.to_string());
        if self.inner.enabled {
            bar.enable_steady_tick(TICK_INTERVAL);
        }
        Step {
            bar,
            multi: Some(self.inner.multi.clone()),
            label: label.to_string(),
            start: Instant::now(),
        }
    }

    /// Start a spinner only in verbose mode. Returns `None` in normal mode.
    pub fn verbose_step(&self, label: &str) -> Option<Step> {
        if self.inner.verbosity.is_verbose() {
            Some(self.step(label))
        } else {
            None
        }
    }

    /// Start a grouped phase with indented child spinners.
    pub fn phase(&self, label: &str) -> Phase {
        let bar = self.inner.multi.add(ProgressBar::new_spinner());
        bar.set_style(spinner_style());
        bar.set_message(label.to_string());
        if self.inner.enabled {
            bar.enable_steady_tick(TICK_INTERVAL);
        }
        Phase {
            parent: bar,
            multi: self.inner.multi.clone(),
            label: label.to_string(),
            start: Instant::now(),
            enabled: self.inner.enabled,
            completed_children: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    /// Convenience: run an async operation with a spinner.
    pub async fn run_step<F, T>(&self, label: &str, f: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        let step = self.step(label);
        let result = f.await;
        step.finish();
        result
    }

    /// Print a warning message in the spinner flow.
    ///
    /// Shows a yellow `⚠` prefix. Always visible regardless of verbosity.
    pub fn warn(&self, msg: &str) {
        let _ = self
            .inner
            .multi
            .println(format!("  \x1b[33m⚠\x1b[0m {msg}"));
    }

    /// Print an error message in the spinner flow.
    ///
    /// Shows a red `✗` prefix. Always visible regardless of verbosity.
    pub fn error(&self, msg: &str) {
        let _ = self
            .inner
            .multi
            .println(format!("  \x1b[31m✗\x1b[0m {msg}"));
    }

    /// Print a hint/fix suggestion in the spinner flow.
    ///
    /// Shows a dim `→` prefix, indented. Always visible regardless of verbosity.
    pub fn hint(&self, msg: &str) {
        let _ = self
            .inner
            .multi
            .println(format!("    \x1b[2m→ {msg}\x1b[0m"));
    }

    /// Print a line through the progress system (appears above spinners).
    ///
    /// Used for streaming lifecycle command output under an active spinner.
    pub fn println(&self, msg: &str) {
        let _ = self.inner.multi.println(msg);
    }
}

// ── Step ─────────────────────────────────────────────────────────────

/// Handle for a single timed operation (one spinner line).
///
/// Top-level steps (created via [`Progress::step`]) finish as permanent
/// terminal output that survives streaming output (docker build, lifecycle).
/// Phase children (created via [`Phase::step`]) finish in-place to preserve
/// parent-above-children ordering.
pub struct Step {
    bar: ProgressBar,
    /// When set, finish prints a permanent line via `MultiProgress::println`
    /// and then clears the ephemeral spinner. This prevents the line from
    /// being scrolled away when streaming output follows.
    multi: Option<MultiProgress>,
    label: String,
    start: Instant,
}

impl Step {
    /// Finish with checkmark and elapsed time.
    pub fn finish(self) {
        let elapsed = self.start.elapsed();
        let time_suffix = format_elapsed(elapsed);
        let msg = format!("\x1b[32m✓\x1b[0m {}{time_suffix}", self.label);
        self.finish_impl(&msg);
    }

    /// Finish with a custom completion message.
    pub fn finish_with(self, msg: &str) {
        let elapsed = self.start.elapsed();
        let time_suffix = format_elapsed(elapsed);
        let line = format!("\x1b[32m✓\x1b[0m {msg}{time_suffix}");
        self.finish_impl(&line);
    }

    /// Mark as failed.
    pub fn fail(self, msg: &str) {
        let line = format!("\x1b[31m✗\x1b[0m {}: {msg}", self.label);
        self.finish_impl(&line);
    }

    fn finish_impl(&self, msg: &str) {
        if let Some(ref multi) = self.multi {
            // Top-level: print permanent line, then clear spinner from managed region.
            let _ = multi.println(format!("  {msg}"));
            self.bar.finish_and_clear();
        } else {
            // Phase child: finish in-place to preserve ordering.
            self.bar.finish_with_message(msg.to_string());
        }
    }
}

impl Drop for Step {
    fn drop(&mut self) {
        // If not yet finished (e.g. early error return), clear the spinner.
        if !self.bar.is_finished() {
            self.bar.finish_and_clear();
        }
    }
}

// ── Phase ────────────────────────────────────────────────────────────

/// Grouped parent spinner that can have indented child steps.
///
/// On finish, prints the parent line followed by all completed child
/// lines as permanent output, then clears all bars from the managed
/// region. This ensures correct ordering (parent above children) AND
/// permanence (lines survive subsequent streaming output).
pub struct Phase {
    parent: ProgressBar,
    multi: MultiProgress,
    label: String,
    start: Instant,
    enabled: bool,
    /// Completed child descriptions, collected as children finish.
    completed_children: Arc<std::sync::Mutex<Vec<String>>>,
}

impl Phase {
    /// Add a child step under this phase (indented spinner).
    pub fn step(&self, label: &str) -> PhaseChild {
        let bar = self
            .multi
            .insert_after(&self.parent, ProgressBar::new_spinner());
        bar.set_style(child_spinner_style());
        bar.set_message(label.to_string());
        if self.enabled {
            bar.enable_steady_tick(TICK_INTERVAL);
        }
        PhaseChild {
            bar,
            label: label.to_string(),
            start: Instant::now(),
            completed: Arc::clone(&self.completed_children),
        }
    }

    /// Finish the phase with total elapsed time.
    ///
    /// Prints parent + children as permanent output in correct order,
    /// then clears all bars from the managed region.
    pub fn finish(self) {
        let elapsed = self.start.elapsed();
        let time_suffix = format_elapsed(elapsed);

        // Print parent line first (permanent).
        let _ = self
            .multi
            .println(format!("  \x1b[32m✓\x1b[0m {}{time_suffix}", self.label));

        // Print completed children in the order they finished (permanent).
        let Ok(children) = self.completed_children.lock() else {
            self.parent.finish_and_clear();
            return;
        };
        for child_line in children.iter() {
            let _ = self.multi.println(child_line);
        }

        self.parent.finish_and_clear();
    }
}

impl Drop for Phase {
    fn drop(&mut self) {
        if !self.parent.is_finished() {
            self.parent.finish_and_clear();
        }
    }
}

/// Handle for a child step within a [`Phase`].
///
/// On finish, records its completion message for the parent Phase
/// to print in order, then clears its spinner.
pub struct PhaseChild {
    bar: ProgressBar,
    label: String,
    start: Instant,
    completed: Arc<std::sync::Mutex<Vec<String>>>,
}

impl PhaseChild {
    /// Finish with checkmark and elapsed time.
    pub fn finish(self) {
        let elapsed = self.start.elapsed();
        let time_suffix = format_elapsed(elapsed);
        let msg = format!("      \x1b[32m✓\x1b[0m {}{time_suffix}", self.label);
        if let Ok(mut children) = self.completed.lock() {
            children.push(msg);
        }
        self.bar.finish_and_clear();
    }
}

impl Drop for PhaseChild {
    fn drop(&mut self) {
        if !self.bar.is_finished() {
            self.bar.finish_and_clear();
        }
    }
}

// ── Tracing writer ───────────────────────────────────────────────────

/// Routes tracing output through [`MultiProgress::println`] so log lines
/// appear above active spinners instead of corrupting them.
#[derive(Clone)]
pub struct IndicatifMakeWriter {
    multi: MultiProgress,
}

impl IndicatifMakeWriter {
    pub const fn new(multi: MultiProgress) -> Self {
        Self { multi }
    }
}

impl<'a> MakeWriter<'a> for IndicatifMakeWriter {
    type Writer = IndicatifLineWriter;

    fn make_writer(&'a self) -> Self::Writer {
        IndicatifLineWriter {
            multi: self.multi.clone(),
            buf: Vec::with_capacity(256),
        }
    }
}

/// Buffers a single tracing event, then flushes via `MultiProgress::println` on drop.
pub struct IndicatifLineWriter {
    multi: MultiProgress,
    buf: Vec<u8>,
}

impl io::Write for IndicatifLineWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if !self.buf.is_empty() {
            let msg = String::from_utf8_lossy(&self.buf);
            let trimmed = msg.trim_end();
            if !trimmed.is_empty() {
                let _ = self.multi.println(trimmed);
            }
            self.buf.clear();
        }
        Ok(())
    }
}

impl Drop for IndicatifLineWriter {
    fn drop(&mut self) {
        let _ = io::Write::flush(self);
    }
}

// ── helpers ──────────────────────────────────────────────────────────

fn format_elapsed(elapsed: Duration) -> String {
    if elapsed.as_millis() >= 100 {
        format!(" ({:.1}s)", elapsed.as_secs_f64())
    } else {
        String::new()
    }
}

/// Public version of [`format_elapsed`] for use by command modules
/// that manually format completion lines (e.g., Docker build output).
pub fn format_elapsed_pub(elapsed: Duration) -> String {
    format_elapsed(elapsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Verbosity ────────────────────────────────────────────────────

    #[test]
    fn verbosity_normal_is_not_verbose() {
        assert!(!Verbosity::Normal.is_verbose());
    }

    #[test]
    fn verbosity_verbose_is_verbose() {
        assert!(Verbosity::Verbose.is_verbose());
    }

    // ── format_elapsed ───────────────────────────────────────────────

    #[test]
    fn format_elapsed_below_threshold_is_empty() {
        let result = format_elapsed(Duration::from_millis(50));
        assert!(result.is_empty());
    }

    #[test]
    fn format_elapsed_at_threshold_shows_time() {
        let result = format_elapsed(Duration::from_millis(100));
        assert_eq!(result, " (0.1s)");
    }

    #[test]
    fn format_elapsed_above_threshold_shows_time() {
        let result = format_elapsed(Duration::from_millis(1500));
        assert_eq!(result, " (1.5s)");
    }

    #[test]
    fn format_elapsed_large_duration() {
        let result = format_elapsed(Duration::from_secs(66));
        assert_eq!(result, " (66.0s)");
    }

    // ── Progress construction ────────────────────────────────────────

    #[test]
    fn progress_disabled_reports_not_enabled() {
        let p = Progress::new(false, Verbosity::Normal);
        assert!(!p.is_enabled());
    }

    #[test]
    fn progress_enabled_reports_enabled() {
        let p = Progress::new(true, Verbosity::Normal);
        assert!(p.is_enabled());
    }

    #[test]
    fn progress_verbosity_is_threaded_through() {
        let p = Progress::new(false, Verbosity::Verbose);
        assert!(p.is_verbose());
        assert_eq!(p.verbosity(), Verbosity::Verbose);
    }

    // ── verbose_step ─────────────────────────────────────────────────

    #[test]
    fn verbose_step_returns_none_in_normal_mode() {
        let p = Progress::new(false, Verbosity::Normal);
        assert!(p.verbose_step("test").is_none());
    }

    #[test]
    fn verbose_step_returns_some_in_verbose_mode() {
        let p = Progress::new(false, Verbosity::Verbose);
        assert!(p.verbose_step("test").is_some());
    }

    // ── warn/error/hint output ───────────────────────────────────────

    #[test]
    fn warn_contains_yellow_triangle() {
        // Progress::warn() writes through MultiProgress::println().
        // With a hidden draw target, the output is discarded, but we
        // can verify the method doesn't panic.
        let p = Progress::new(false, Verbosity::Normal);
        p.warn("test warning");
    }

    #[test]
    fn error_contains_red_x() {
        let p = Progress::new(false, Verbosity::Normal);
        p.error("test error");
    }

    #[test]
    fn hint_contains_dim_arrow() {
        let p = Progress::new(false, Verbosity::Normal);
        p.hint("try this fix");
    }

    // ── IndicatifLineWriter ──────────────────────────────────────────

    #[test]
    fn indicatif_line_writer_buffers_and_flushes() {
        use std::io::Write;
        let multi = MultiProgress::with_draw_target(indicatif::ProgressDrawTarget::hidden());
        let mut writer = IndicatifLineWriter {
            multi,
            buf: Vec::new(),
        };
        writer.write_all(b"hello world\n").unwrap();
        writer.flush().unwrap();
        // Buffer should be cleared after flush
        assert!(writer.buf.is_empty());
    }

    #[test]
    fn indicatif_line_writer_empty_flush_is_noop() {
        use std::io::Write;
        let multi = MultiProgress::with_draw_target(indicatif::ProgressDrawTarget::hidden());
        let mut writer = IndicatifLineWriter {
            multi,
            buf: Vec::new(),
        };
        writer.flush().unwrap();
        assert!(writer.buf.is_empty());
    }

    // ── Snapshot tests for output formatting ─────────────────────────

    #[test]
    fn snapshot_format_elapsed_values() {
        let cases = [
            (Duration::from_millis(0), "0ms"),
            (Duration::from_millis(50), "50ms"),
            (Duration::from_millis(99), "99ms"),
            (Duration::from_millis(100), "100ms"),
            (Duration::from_millis(500), "500ms"),
            (Duration::from_secs(1), "1s"),
            (Duration::from_secs(10), "10s"),
            (Duration::from_secs(66), "66s"),
        ];

        let results: Vec<String> = cases
            .iter()
            .map(|(dur, label)| {
                let formatted = format_elapsed(*dur);
                let display = if formatted.is_empty() {
                    "(empty)".to_string()
                } else {
                    formatted
                };
                format!("{label}: {display}")
            })
            .collect();

        insta::assert_snapshot!(results.join("\n"), @r"
        0ms: (empty)
        50ms: (empty)
        99ms: (empty)
        100ms:  (0.1s)
        500ms:  (0.5s)
        1s:  (1.0s)
        10s:  (10.0s)
        66s:  (66.0s)
        ");
    }
}
