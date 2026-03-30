//! Background task manager for in-container worktree operations.
//!
//! Tracks `cella task run` background processes: their Docker exec handles,
//! output capture, and lifecycle state. Tasks are identified by branch name.

use std::collections::HashMap;
use std::fmt::Write;
use std::sync::Arc;

use tokio::sync::Mutex;
use tracing::{info, warn};

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
    /// Captured output (stdout + stderr interleaved).
    output: Arc<Mutex<String>>,
    /// Set when the task completes.
    exit_code: Arc<Mutex<Option<i32>>>,
    /// Handle to abort the task.
    abort_handle: tokio::task::AbortHandle,
    /// Broadcast channel for live output streaming.
    output_tx: tokio::sync::broadcast::Sender<String>,
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
        if self.tasks.contains_key(branch) {
            return Err(format!("task already running for branch '{branch}'"));
        }

        let task_id = branch.to_string();
        let output = Arc::new(Mutex::new(String::new()));
        let exit_code: Arc<Mutex<Option<i32>>> = Arc::new(Mutex::new(None));
        let (output_tx, _) = tokio::sync::broadcast::channel(1024);

        let output_clone = output.clone();
        let exit_code_clone = exit_code.clone();
        let output_tx_clone = output_tx.clone();
        let container = container_name.clone();
        let cmd = command.clone();

        let handle = tokio::spawn(async move {
            run_task_process(
                &container,
                &cmd,
                &output_clone,
                &exit_code_clone,
                &output_tx_clone,
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
            abort_handle: handle.abort_handle(),
            output_tx,
        };

        self.tasks.insert(task_id.clone(), state);
        info!("Started task '{task_id}'");
        Ok(task_id)
    }

    /// List all tasks with their current info.
    pub async fn list_tasks(&self) -> Vec<TaskInfo> {
        let mut infos = Vec::with_capacity(self.tasks.len());
        for (id, state) in &self.tasks {
            let exit = *state.exit_code.lock().await;
            infos.push(TaskInfo {
                task_id: id.clone(),
                branch: state.branch.clone(),
                container_name: state.container_name.clone(),
                command: state.command.clone(),
                elapsed_secs: state.started_at.elapsed().as_secs(),
                is_done: exit.is_some(),
                exit_code: exit,
            });
        }
        infos
    }

    /// Subscribe to live output for a task (for `--follow` mode).
    pub fn subscribe(&self, branch: &str) -> Option<tokio::sync::broadcast::Receiver<String>> {
        self.tasks.get(branch).map(|s| s.output_tx.subscribe())
    }

    /// Check if a task is done.
    pub async fn is_done(&self, branch: &str) -> bool {
        if let Some(state) = self.tasks.get(branch) {
            state.exit_code.lock().await.is_some()
        } else {
            true // no task = done
        }
    }

    /// Get captured output for a task.
    pub async fn get_output(&self, branch: &str) -> Option<String> {
        let state = self.tasks.get(branch)?;
        Some(state.output.lock().await.clone())
    }

    /// Wait for a task to complete, returning its exit code.
    pub async fn wait_for(&self, branch: &str) -> Option<i32> {
        let state = self.tasks.get(branch)?;
        let exit_code = state.exit_code.clone();
        // Poll until done
        loop {
            let code = *exit_code.lock().await;
            if let Some(c) = code {
                return Some(c);
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    }

    /// Stop a running task.
    pub async fn stop_task(&mut self, branch: &str) -> bool {
        if let Some(state) = self.tasks.get(branch) {
            state.abort_handle.abort();
            // Set exit code to signal interrupted
            *state.exit_code.lock().await = Some(130); // SIGINT convention
            info!("Stopped task '{branch}'");
            true
        } else {
            false
        }
    }

    /// Remove completed tasks.
    pub async fn cleanup_done(&mut self) {
        let mut to_remove = Vec::new();
        for (id, state) in &self.tasks {
            if state.exit_code.lock().await.is_some() {
                to_remove.push(id.clone());
            }
        }
        for id in to_remove {
            self.tasks.remove(&id);
        }
    }
}

/// Run a docker exec command and capture output.
async fn run_task_process(
    container_name: &str,
    command: &[String],
    output: &Arc<Mutex<String>>,
    exit_code: &Arc<Mutex<Option<i32>>>,
    output_tx: &tokio::sync::broadcast::Sender<String>,
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
            *exit_code.lock().await = Some(1);
            let error_line = format!("Error: failed to spawn: {e}");
            let _ = writeln!(output.lock().await, "{error_line}");
            let _ = output_tx.send(format!("{error_line}\n"));
            return;
        }
    };

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
                output_stdout.lock().await.push_str(&formatted);
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
                output_stderr.lock().await.push_str(&formatted);
                let _ = tx_stderr.send(formatted);
            }
        }
    });

    let _ = stdout_task.await;
    let _ = stderr_task.await;

    let status = child.wait().await;
    let code = status.map_or(1, |s| s.code().unwrap_or(1));
    *exit_code.lock().await = Some(code);
    info!("Task in '{container_name}' exited with code {code}");
}
