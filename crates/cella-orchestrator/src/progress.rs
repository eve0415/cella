//! Progress reporting for orchestrator operations.
//!
//! The orchestrator emits [`ProgressEvent`] values onto a channel.
//! Consumers decide how to present them:
//! - CLI: renders as indicatif spinners
//! - Daemon: serializes as TCP `DaemonMessage` variants

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// A single progress event emitted by orchestrator operations.
#[derive(Debug, Clone)]
pub enum ProgressEvent {
    /// A top-level step has started.
    StepStarted { id: u64, label: String },

    /// A step completed successfully.
    StepCompleted { id: u64, elapsed: Duration },

    /// A step completed with a custom message replacing the label.
    StepCompletedWith {
        id: u64,
        message: String,
        elapsed: Duration,
    },

    /// A step failed.
    StepFailed { id: u64, message: String },

    /// A grouped phase started (has child steps).
    PhaseStarted { id: u64, label: String },

    /// A child step within a phase started.
    PhaseChildStarted {
        parent_id: u64,
        id: u64,
        label: String,
    },

    /// A phase child completed.
    PhaseChildCompleted {
        parent_id: u64,
        id: u64,
        elapsed: Duration,
    },

    /// A phase finished.
    PhaseCompleted { id: u64, elapsed: Duration },

    /// Warning message.
    Warn { message: String },

    /// Hint / suggestion.
    Hint { message: String },

    /// Line of streaming output (docker build, lifecycle commands).
    Output { line: String },

    /// Error message.
    Error { message: String },
}

/// Sends [`ProgressEvent`] values to a consumer.
///
/// Wraps a `tokio::sync::mpsc::Sender` with convenience methods that
/// mirror the `Progress` API used in the CLI.
#[derive(Clone)]
pub struct ProgressSender {
    tx: tokio::sync::mpsc::Sender<ProgressEvent>,
    next_id: Arc<AtomicU64>,
    verbose: bool,
}

impl ProgressSender {
    /// Create a new sender.
    pub fn new(tx: tokio::sync::mpsc::Sender<ProgressEvent>, verbose: bool) -> Self {
        Self {
            tx,
            next_id: Arc::new(AtomicU64::new(1)),
            verbose,
        }
    }

    /// Whether verbose mode is active.
    pub const fn is_verbose(&self) -> bool {
        self.verbose
    }

    fn alloc_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Start a top-level step. Returns a handle to complete it.
    pub fn step(&self, label: &str) -> StepHandle {
        let id = self.alloc_id();
        let _ = self.tx.try_send(ProgressEvent::StepStarted {
            id,
            label: label.to_string(),
        });
        StepHandle {
            id,
            tx: self.tx.clone(),
            start: std::time::Instant::now(),
            finished: false,
        }
    }

    /// Start a step only in verbose mode. Returns `None` in normal mode.
    pub fn verbose_step(&self, label: &str) -> Option<StepHandle> {
        if self.verbose {
            Some(self.step(label))
        } else {
            None
        }
    }

    /// Start a grouped phase. Returns a handle for adding children.
    pub fn phase(&self, label: &str) -> PhaseHandle {
        let id = self.alloc_id();
        let _ = self.tx.try_send(ProgressEvent::PhaseStarted {
            id,
            label: label.to_string(),
        });
        PhaseHandle {
            id,
            tx: self.tx.clone(),
            next_id: Arc::clone(&self.next_id),
            start: std::time::Instant::now(),
            finished: false,
        }
    }

    /// Emit a warning.
    pub fn warn(&self, msg: &str) {
        let _ = self.tx.try_send(ProgressEvent::Warn {
            message: msg.to_string(),
        });
    }

    /// Emit a hint / suggestion.
    pub fn hint(&self, msg: &str) {
        let _ = self.tx.try_send(ProgressEvent::Hint {
            message: msg.to_string(),
        });
    }

    /// Emit a line of streaming output.
    pub fn println(&self, msg: &str) {
        let _ = self.tx.try_send(ProgressEvent::Output {
            line: msg.to_string(),
        });
    }

    /// Emit an error message.
    pub fn error(&self, msg: &str) {
        let _ = self.tx.try_send(ProgressEvent::Error {
            message: msg.to_string(),
        });
    }
}

/// Handle for a single step. Sends completion on finish/drop.
pub struct StepHandle {
    id: u64,
    tx: tokio::sync::mpsc::Sender<ProgressEvent>,
    start: std::time::Instant,
    finished: bool,
}

impl StepHandle {
    /// Finish with success.
    pub fn finish(mut self) {
        self.finished = true;
        let _ = self.tx.try_send(ProgressEvent::StepCompleted {
            id: self.id,
            elapsed: self.start.elapsed(),
        });
    }

    /// Finish with a custom message.
    pub fn finish_with(mut self, msg: &str) {
        self.finished = true;
        let _ = self.tx.try_send(ProgressEvent::StepCompletedWith {
            id: self.id,
            message: msg.to_string(),
            elapsed: self.start.elapsed(),
        });
    }

    /// Mark as failed.
    pub fn fail(mut self, msg: &str) {
        self.finished = true;
        let _ = self.tx.try_send(ProgressEvent::StepFailed {
            id: self.id,
            message: msg.to_string(),
        });
    }
}

impl Drop for StepHandle {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.tx.try_send(ProgressEvent::StepFailed {
                id: self.id,
                message: "dropped without finishing".to_string(),
            });
        }
    }
}

/// Handle for a grouped phase. Supports child steps.
pub struct PhaseHandle {
    id: u64,
    tx: tokio::sync::mpsc::Sender<ProgressEvent>,
    next_id: Arc<AtomicU64>,
    start: std::time::Instant,
    finished: bool,
}

impl PhaseHandle {
    /// Add a child step within this phase.
    pub fn step(&self, label: &str) -> PhaseChildHandle {
        let child_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let _ = self.tx.try_send(ProgressEvent::PhaseChildStarted {
            parent_id: self.id,
            id: child_id,
            label: label.to_string(),
        });
        PhaseChildHandle {
            parent_id: self.id,
            id: child_id,
            tx: self.tx.clone(),
            start: std::time::Instant::now(),
            finished: false,
        }
    }

    /// Finish the phase.
    pub fn finish(mut self) {
        self.finished = true;
        let _ = self.tx.try_send(ProgressEvent::PhaseCompleted {
            id: self.id,
            elapsed: self.start.elapsed(),
        });
    }
}

impl Drop for PhaseHandle {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.tx.try_send(ProgressEvent::PhaseCompleted {
                id: self.id,
                elapsed: self.start.elapsed(),
            });
        }
    }
}

/// Handle for a child step within a phase.
pub struct PhaseChildHandle {
    parent_id: u64,
    id: u64,
    tx: tokio::sync::mpsc::Sender<ProgressEvent>,
    start: std::time::Instant,
    finished: bool,
}

impl PhaseChildHandle {
    /// Finish with success.
    pub fn finish(mut self) {
        self.finished = true;
        let _ = self.tx.try_send(ProgressEvent::PhaseChildCompleted {
            parent_id: self.parent_id,
            id: self.id,
            elapsed: self.start.elapsed(),
        });
    }
}

impl Drop for PhaseChildHandle {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.tx.try_send(ProgressEvent::PhaseChildCompleted {
                parent_id: self.parent_id,
                id: self.id,
                elapsed: self.start.elapsed(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn step_sends_started_and_completed() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(32);
        let sender = ProgressSender::new(tx, false);

        let step = sender.step("Building image");
        step.finish();

        let ev1 = rx.recv().await.unwrap();
        assert!(matches!(ev1, ProgressEvent::StepStarted { id: 1, .. }));

        let ev2 = rx.recv().await.unwrap();
        assert!(matches!(ev2, ProgressEvent::StepCompleted { id: 1, .. }));
    }

    #[tokio::test]
    async fn phase_with_children() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(32);
        let sender = ProgressSender::new(tx, false);

        let phase = sender.phase("Installing features");
        let child = phase.step("git");
        child.finish();
        phase.finish();

        let ev1 = rx.recv().await.unwrap();
        assert!(matches!(ev1, ProgressEvent::PhaseStarted { id: 1, .. }));

        let ev2 = rx.recv().await.unwrap();
        assert!(matches!(
            ev2,
            ProgressEvent::PhaseChildStarted {
                parent_id: 1,
                id: 2,
                ..
            }
        ));

        let ev3 = rx.recv().await.unwrap();
        assert!(matches!(
            ev3,
            ProgressEvent::PhaseChildCompleted {
                parent_id: 1,
                id: 2,
                ..
            }
        ));

        let ev4 = rx.recv().await.unwrap();
        assert!(matches!(ev4, ProgressEvent::PhaseCompleted { id: 1, .. }));
    }

    #[tokio::test]
    async fn dropped_step_sends_failed() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(32);
        let sender = ProgressSender::new(tx, false);

        {
            let _step = sender.step("will drop");
        }

        let _ = rx.recv().await.unwrap(); // StepStarted
        let ev = rx.recv().await.unwrap();
        assert!(matches!(ev, ProgressEvent::StepFailed { id: 1, .. }));
    }

    #[tokio::test]
    async fn verbose_step_none_in_normal_mode() {
        let (tx, _rx) = tokio::sync::mpsc::channel(32);
        let sender = ProgressSender::new(tx, false);
        assert!(sender.verbose_step("hidden").is_none());
    }

    #[tokio::test]
    async fn verbose_step_some_in_verbose_mode() {
        let (tx, _rx) = tokio::sync::mpsc::channel(32);
        let sender = ProgressSender::new(tx, true);
        assert!(sender.verbose_step("visible").is_some());
    }
}
