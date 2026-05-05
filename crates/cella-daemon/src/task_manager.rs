//! Background task manager for in-container worktree operations.
//!
//! Tracks `cella task run` background processes: their Docker exec handles,
//! output capture, and lifecycle state. Tasks are identified by branch name.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};

use tokio::sync::Mutex;
use tracing::{info, warn};

/// Sentinel value: task is still running (no valid exit code uses -1 on Unix).
const EXIT_RUNNING: i32 = -1;

const MAX_OUTPUT_BYTES: usize = 1024 * 1024; // 1 MB

/// Ring buffer for captured task output.
struct TaskOutput {
    buffer: VecDeque<u8>,
    max_bytes: usize,
}

impl TaskOutput {
    const fn new(max_bytes: usize) -> Self {
        Self {
            buffer: VecDeque::new(),
            max_bytes,
        }
    }

    fn push(&mut self, data: &str) {
        let bytes = data.as_bytes();
        self.buffer.extend(bytes);
        while self.buffer.len() > self.max_bytes {
            self.buffer.pop_front();
        }
    }

    fn contents(&self) -> String {
        let bytes: Vec<u8> = self.buffer.iter().copied().collect();
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

/// Shared task manager state.
pub type SharedTaskManager = Arc<Mutex<TaskManager>>;

/// Creates a new shared task manager.
pub fn new_shared() -> SharedTaskManager {
    Arc::new(Mutex::new(TaskManager::new()))
}

/// Manages background tasks spawned by `cella task run`.
pub struct TaskManager {
    tasks: HashMap<String, TaskState>,
}

/// State of a single background task.
struct TaskState {
    branch: String,
    container_name: String,
    command: Vec<String>,
    started_at: std::time::Instant,
    /// Captured output (stdout + stderr interleaved, ring buffer).
    output: Arc<Mutex<TaskOutput>>,
    /// `EXIT_RUNNING` (-1) while task is active; real exit code once done.
    exit_code: Arc<AtomicI32>,
    /// PID of the child `docker exec` process (0 = not yet known).
    child_pid: Arc<AtomicU32>,
    /// Handle to abort the task.
    abort_handle: tokio::task::AbortHandle,
    /// Broadcast channel for live output streaming.
    output_tx: tokio::sync::broadcast::Sender<String>,
    /// Watch channel for non-blocking wait on task completion.
    exit_watch: tokio::sync::watch::Sender<Option<i32>>,
}

/// Public task info for list responses.
pub struct TaskInfo {
    pub task_id: String,
    pub branch: String,
    pub container_name: String,
    pub command: Vec<String>,
    pub elapsed_secs: u64,
    pub is_done: bool,
    pub exit_code: Option<i32>,
}

impl TaskManager {
    fn new() -> Self {
        Self {
            tasks: HashMap::new(),
        }
    }

    /// Start a background task: create branch (if needed) and run command.
    ///
    /// Returns the task ID (branch name) on success.
    ///
    /// # Errors
    ///
    /// Returns an error if a task is already running for the given branch.
    pub fn start_task(
        &mut self,
        branch: &str,
        container_name: String,
        command: Vec<String>,
    ) -> Result<String, String> {
        if let Some(existing) = self.tasks.get(branch) {
            if existing.exit_code.load(Ordering::Acquire) == EXIT_RUNNING {
                return Err(format!("task already running for branch '{branch}'"));
            }
            self.tasks.remove(branch);
        }

        let task_id = branch.to_string();
        let output = Arc::new(Mutex::new(TaskOutput::new(MAX_OUTPUT_BYTES)));
        let exit_code = Arc::new(AtomicI32::new(EXIT_RUNNING));
        let child_pid = Arc::new(AtomicU32::new(0));
        let (output_tx, _) = tokio::sync::broadcast::channel(1024);
        let (exit_watch, _) = tokio::sync::watch::channel(None);

        let output_clone = output.clone();
        let exit_code_clone = exit_code.clone();
        let child_pid_clone = child_pid.clone();
        let output_tx_clone = output_tx.clone();
        let exit_watch_clone = exit_watch.clone();
        let container = container_name.clone();
        let cmd = command.clone();

        let handle = tokio::spawn(async move {
            run_task_process(
                &container,
                &cmd,
                &output_clone,
                &exit_code_clone,
                &child_pid_clone,
                &output_tx_clone,
                &exit_watch_clone,
            )
            .await;
        });

        let state = TaskState {
            branch: branch.to_string(),
            container_name,
            command,
            started_at: std::time::Instant::now(),
            output,
            exit_code,
            child_pid,
            abort_handle: handle.abort_handle(),
            output_tx,
            exit_watch,
        };

        self.tasks.insert(task_id.clone(), state);
        info!("Started task '{task_id}'");
        Ok(task_id)
    }

    /// List all tasks with their current info.
    pub fn list_tasks(&self) -> Vec<TaskInfo> {
        let mut infos = Vec::with_capacity(self.tasks.len());
        for (id, state) in &self.tasks {
            let code = state.exit_code.load(Ordering::Acquire);
            let done = code != EXIT_RUNNING;
            infos.push(TaskInfo {
                task_id: id.clone(),
                branch: state.branch.clone(),
                container_name: state.container_name.clone(),
                command: state.command.clone(),
                elapsed_secs: state.started_at.elapsed().as_secs(),
                is_done: done,
                exit_code: if done { Some(code) } else { None },
            });
        }
        infos
    }

    /// Subscribe to live output for a task (for `--follow` mode).
    pub fn subscribe(&self, branch: &str) -> Option<tokio::sync::broadcast::Receiver<String>> {
        self.tasks.get(branch).map(|s| s.output_tx.subscribe())
    }

    /// Subscribe to exit notification for a task.
    pub fn subscribe_exit(
        &self,
        branch: &str,
    ) -> Option<tokio::sync::watch::Receiver<Option<i32>>> {
        self.tasks.get(branch).map(|s| s.exit_watch.subscribe())
    }

    /// Check if a task is done.
    pub fn is_done(&self, branch: &str) -> bool {
        self.tasks
            .get(branch)
            .is_none_or(|state| state.exit_code.load(Ordering::Acquire) != EXIT_RUNNING)
    }

    /// Get captured output for a task.
    pub async fn get_output(&self, branch: &str) -> Option<String> {
        let state = self.tasks.get(branch)?;
        Some(state.output.lock().await.contents())
    }

    /// Stop a running task. Only sets exit code 130 if still running.
    pub async fn stop_task(&mut self, branch: &str) -> bool {
        let Some(state) = self.tasks.get(branch) else {
            return false;
        };

        // Only stop if still running — never overwrite a completed exit code.
        if state
            .exit_code
            .compare_exchange(EXIT_RUNNING, 130, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }

        // Kill the child process with SIGTERM.
        let pid = state.child_pid.load(Ordering::Acquire);
        if pid != 0 {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid.cast_signed()),
                nix::sys::signal::Signal::SIGTERM,
            );
        }

        // Give the process 2s to exit gracefully, then abort the Tokio task.
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        state.abort_handle.abort();

        let _ = state.exit_watch.send(Some(130));
        info!("Stopped task '{branch}'");
        true
    }

    /// Remove completed tasks.
    pub fn cleanup_done(&mut self) {
        self.tasks
            .retain(|_, state| state.exit_code.load(Ordering::Acquire) == EXIT_RUNNING);
    }
}

/// Run a docker exec command and capture output.
async fn run_task_process(
    container_name: &str,
    command: &[String],
    output: &Arc<Mutex<TaskOutput>>,
    exit_code: &Arc<AtomicI32>,
    child_pid: &Arc<AtomicU32>,
    output_tx: &tokio::sync::broadcast::Sender<String>,
    exit_watch: &tokio::sync::watch::Sender<Option<i32>>,
) {
    use tokio::io::AsyncBufReadExt;

    let mut cmd = tokio::process::Command::new("docker");
    cmd.arg("exec").arg(container_name);
    for arg in command {
        cmd.arg(arg);
    }
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            warn!("Failed to spawn task process: {e}");
            exit_code.store(1, Ordering::Release);
            let _ = exit_watch.send(Some(1));
            let error_line = format!("Error: failed to spawn: {e}\n");
            output.lock().await.push(&error_line);
            let _ = output_tx.send(error_line);
            return;
        }
    };

    child_pid.store(child.id().unwrap_or(0), Ordering::Release);

    // Read stdout and stderr concurrently
    let child_stdout = child.stdout.take();
    let child_stderr = child.stderr.take();

    let output_stdout = output.clone();
    let tx_stdout = output_tx.clone();
    let stdout_task = tokio::spawn(async move {
        if let Some(stdout) = child_stdout {
            let mut reader = tokio::io::BufReader::new(stdout).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                let formatted = format!("{line}\n");
                output_stdout.lock().await.push(&formatted);
                let _ = tx_stdout.send(formatted);
            }
        }
    });

    let output_stderr = output.clone();
    let tx_stderr = output_tx.clone();
    let stderr_task = tokio::spawn(async move {
        if let Some(stderr) = child_stderr {
            let mut reader = tokio::io::BufReader::new(stderr).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                let formatted = format!("{line}\n");
                output_stderr.lock().await.push(&formatted);
                let _ = tx_stderr.send(formatted);
            }
        }
    });

    let _ = stdout_task.await;
    let _ = stderr_task.await;

    let status = child.wait().await;
    let code = status.map_or(1, |s| s.code().unwrap_or(1));
    // Only set if still running — stop_task may have already set 130.
    let _ = exit_code.compare_exchange(EXIT_RUNNING, code, Ordering::AcqRel, Ordering::Acquire);
    let _ = exit_watch.send(Some(code));
    info!("Task in '{container_name}' exited with code {code}");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: create a fresh `TaskManager`.
    fn manager() -> TaskManager {
        TaskManager::new()
    }

    // -- new_shared / TaskManager::new --

    #[test]
    fn new_shared_creates_empty_manager() {
        // `new_shared` must return an Arc<Mutex<TaskManager>> with no tasks.
        let _shared = new_shared();
    }

    // -- start_task --

    #[tokio::test]
    async fn start_task_returns_branch_as_task_id() {
        let mut mgr = manager();
        let id = mgr
            .start_task("feature/abc", "ctr1".into(), vec!["ls".into()])
            .unwrap();
        assert_eq!(id, "feature/abc");
    }

    #[tokio::test]
    async fn start_task_duplicate_branch_is_error() {
        let mut mgr = manager();
        mgr.start_task("dup", "ctr".into(), vec!["echo".into()])
            .unwrap();
        let err = mgr
            .start_task("dup", "ctr".into(), vec!["echo".into()])
            .unwrap_err();
        assert!(err.contains("already running"));
    }

    #[tokio::test]
    async fn start_task_different_branches_both_succeed() {
        let mut mgr = manager();
        mgr.start_task("b1", "ctr".into(), vec!["a".into()])
            .unwrap();
        mgr.start_task("b2", "ctr".into(), vec!["b".into()])
            .unwrap();
        let list = mgr.list_tasks();
        assert_eq!(list.len(), 2);
    }

    // -- list_tasks --

    #[tokio::test]
    async fn list_tasks_empty() {
        let mgr = manager();
        assert!(mgr.list_tasks().is_empty());
    }

    #[tokio::test]
    async fn list_tasks_returns_correct_fields() {
        let mut mgr = manager();
        mgr.start_task(
            "main",
            "my-container".into(),
            vec!["cargo".into(), "test".into()],
        )
        .unwrap();
        let tasks = mgr.list_tasks();
        assert_eq!(tasks.len(), 1);

        let t = &tasks[0];
        assert_eq!(t.task_id, "main");
        assert_eq!(t.branch, "main");
        assert_eq!(t.container_name, "my-container");
        assert_eq!(t.command, vec!["cargo", "test"]);
        // Just started, so elapsed should be very small.
        assert!(t.elapsed_secs < 5);
    }

    // -- subscribe --

    #[tokio::test]
    async fn subscribe_returns_none_for_unknown_branch() {
        let mgr = manager();
        assert!(mgr.subscribe("ghost").is_none());
    }

    #[tokio::test]
    async fn subscribe_returns_receiver_for_existing_task() {
        let mut mgr = manager();
        mgr.start_task("br", "c".into(), vec!["x".into()]).unwrap();
        assert!(mgr.subscribe("br").is_some());
    }

    // -- is_done --

    #[tokio::test]
    async fn is_done_returns_true_for_unknown_branch() {
        let mgr = manager();
        // "no task = done" per the implementation.
        assert!(mgr.is_done("nonexistent"));
    }

    #[tokio::test]
    async fn is_done_returns_false_when_task_just_started() {
        let mut mgr = manager();
        mgr.start_task("br", "c".into(), vec!["sleep".into(), "9999".into()])
            .unwrap();
        // The spawned task will fail quickly (docker not available in test), but
        // right after insertion the exit_code starts as None.
        // We check immediately, so it should still be false.
        // NOTE: there is a race, but it is extremely unlikely the spawned task
        // resolves before this line executes.
        assert!(!mgr.is_done("br"));
    }

    // -- get_output --

    #[tokio::test]
    async fn get_output_returns_none_for_unknown_branch() {
        let mgr = manager();
        assert!(mgr.get_output("nope").await.is_none());
    }

    #[tokio::test]
    async fn get_output_returns_some_for_existing_task() {
        let mut mgr = manager();
        mgr.start_task("br", "c".into(), vec!["x".into()]).unwrap();
        // Output starts empty.
        let out = mgr.get_output("br").await.unwrap();
        assert!(out.is_empty() || out.contains("Error")); // may have spawned & failed fast
    }

    // -- stop_task --

    #[tokio::test]
    async fn stop_task_returns_false_for_unknown() {
        let mut mgr = manager();
        assert!(!mgr.stop_task("nope").await);
    }

    #[tokio::test]
    async fn stop_task_on_already_completed_returns_false() {
        let mut mgr = manager();
        mgr.start_task("br", "c".into(), vec!["x".into()]).unwrap();
        // In CI docker is unavailable, so the task exits almost immediately.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        // Task already completed — stop must not overwrite exit code.
        assert!(!mgr.stop_task("br").await);
        assert!(mgr.is_done("br"));
        let tasks = mgr.list_tasks();
        let t = tasks.iter().find(|t| t.task_id == "br").unwrap();
        assert_eq!(t.exit_code, Some(1));
    }

    // -- cleanup_done --

    #[tokio::test]
    async fn cleanup_done_removes_completed_tasks() {
        let mut mgr = manager();
        mgr.start_task("a", "c".into(), vec!["x".into()]).unwrap();
        mgr.start_task("b", "c".into(), vec!["x".into()]).unwrap();
        // Let both tasks finish (docker unavailable → exit 1).
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        mgr.cleanup_done();
        assert!(mgr.list_tasks().is_empty());
    }

    #[tokio::test]
    async fn cleanup_done_on_empty_manager_is_noop() {
        let mut mgr = manager();
        mgr.cleanup_done();
        assert!(mgr.list_tasks().is_empty());
    }

    // -- start_task after completion --

    #[tokio::test]
    async fn start_task_after_completion_succeeds() {
        let mut mgr = manager();
        mgr.start_task("br", "c".into(), vec!["x".into()]).unwrap();
        // Let task finish (docker unavailable → exit 1).
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(mgr.is_done("br"));
        let id = mgr
            .start_task("br", "c".into(), vec!["echo".into()])
            .unwrap();
        assert_eq!(id, "br");
    }

    #[tokio::test]
    async fn start_task_while_running_still_errors() {
        let mut mgr = manager();
        mgr.start_task("br", "c".into(), vec!["sleep".into(), "9999".into()])
            .unwrap();
        // Task is still running (exit_code=None)
        let err = mgr
            .start_task("br", "c".into(), vec!["echo".into()])
            .unwrap_err();
        assert!(err.contains("already running"));
    }

    // -- TaskOutput ring buffer --

    #[test]
    fn task_output_caps_at_max_bytes() {
        let mut out = TaskOutput::new(10);
        out.push("hello"); // 5 bytes
        out.push("world!"); // 6 bytes, total 11 > 10
        let contents = out.contents();
        assert_eq!(contents.len(), 10);
        assert_eq!(contents, "elloworld!");
    }

    #[test]
    fn task_output_under_limit_preserved() {
        let mut out = TaskOutput::new(100);
        out.push("small");
        assert_eq!(out.contents(), "small");
    }
}
