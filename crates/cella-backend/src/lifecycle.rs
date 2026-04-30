//! Lifecycle command parsing and execution.
//!
//! Moved from `cella-docker` so that `cella-orchestrator` can use these
//! types and functions without depending on a concrete backend crate.

use std::io;

use serde_json::Value;
use sha2::{Digest, Sha256};
use tracing::{debug, info};

use crate::error::BackendError;
use crate::progress::{ProgressSender, format_elapsed};
use crate::traits::ContainerBackend;
use crate::types::{ExecOptions, ExecResult};

/// Callback applied to lifecycle entries after resolution, before execution.
pub type PostResolveFn = dyn Fn(&mut Vec<cella_features::LifecycleEntry>) + Send + Sync;

/// Parsed lifecycle command.
pub enum ParsedLifecycle {
    /// Sequential commands.
    Sequential(Vec<Vec<String>>),
    /// Named commands to run in parallel.
    Parallel(Vec<(String, Vec<String>)>),
}

/// Parse a lifecycle command value into executable commands.
///
/// Handles: string → shell command, array → direct command, object → parallel named commands.
pub fn parse_lifecycle_command(value: &Value) -> ParsedLifecycle {
    match value {
        Value::String(s) => {
            ParsedLifecycle::Sequential(vec![vec!["sh".to_string(), "-c".to_string(), s.clone()]])
        }
        Value::Array(arr) => {
            let cmd: Vec<String> = arr
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect();
            ParsedLifecycle::Sequential(vec![cmd])
        }
        Value::Object(map) => {
            let commands: Vec<(String, Vec<String>)> = map
                .iter()
                .map(|(name, v)| {
                    let cmd = match v {
                        Value::String(s) => {
                            vec!["sh".to_string(), "-c".to_string(), s.clone()]
                        }
                        Value::Array(arr) => arr
                            .iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect(),
                        _ => vec!["sh".to_string(), "-c".to_string(), v.to_string()],
                    };
                    (name.clone(), cmd)
                })
                .collect();
            ParsedLifecycle::Parallel(commands)
        }
        _ => ParsedLifecycle::Sequential(vec![]),
    }
}

/// Callback type for routing lifecycle output through a progress system.
pub type OutputCallback<'a> = Box<dyn Fn(&str) + Send + Sync + 'a>;

/// Shared container context for lifecycle phase execution.
pub struct LifecycleContext<'a> {
    /// Container backend (trait object).
    pub client: &'a dyn ContainerBackend,
    /// Container to run commands in.
    pub container_id: &'a str,
    /// User to run commands as.
    pub user: Option<&'a str>,
    /// Environment variables.
    pub env: &'a [String],
    /// Working directory inside the container.
    pub working_dir: Option<&'a str>,
    /// Whether to print progress and stream output to stderr.
    pub is_text: bool,
    /// Optional callback for routing output lines through a progress system.
    ///
    /// When set, sequential lifecycle output is written through this callback
    /// (e.g., indented under an active spinner) instead of directly to stderr.
    pub on_output: Option<OutputCallback<'a>>,
}

/// A `Write` adapter that buffers lines and forwards each complete line
/// through a callback with indentation.
struct CallbackWriter<'a> {
    callback: &'a (dyn Fn(&str) + Send + Sync),
    buf: Vec<u8>,
}

impl<'a> CallbackWriter<'a> {
    fn new(callback: &'a (dyn Fn(&str) + Send + Sync)) -> Self {
        Self {
            callback,
            buf: Vec::with_capacity(256),
        }
    }

    fn flush_lines(&mut self) {
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let line = String::from_utf8_lossy(&self.buf[..pos]);
            if !line.trim().is_empty() {
                (self.callback)(&format!("      {line}"));
            }
            self.buf.drain(..=pos);
        }
    }

    fn flush_remaining(&mut self) {
        if !self.buf.is_empty() {
            let line = String::from_utf8_lossy(&self.buf);
            if !line.trim().is_empty() {
                (self.callback)(&format!("      {line}"));
            }
            self.buf.clear();
        }
    }
}

impl io::Write for CallbackWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(buf);
        self.flush_lines();
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flush_remaining();
        Ok(())
    }
}

impl Drop for CallbackWriter<'_> {
    fn drop(&mut self) {
        self.flush_remaining();
    }
}

/// Run sequential lifecycle commands, streaming output when `is_text`.
async fn run_sequential(
    ctx: &LifecycleContext<'_>,
    phase: &str,
    commands: Vec<Vec<String>>,
) -> Result<(), BackendError> {
    for cmd in commands {
        if cmd.is_empty() {
            continue;
        }
        debug!("{phase}: {}", cmd.join(" "));
        let opts = ExecOptions {
            cmd,
            user: ctx.user.map(String::from),
            env: Some(ctx.env.to_vec()),
            working_dir: ctx.working_dir.map(String::from),
        };
        let result = if ctx.is_text {
            if let Some(ref on_output) = ctx.on_output {
                // Route through progress system with indentation
                ctx.client
                    .exec_stream(
                        ctx.container_id,
                        &opts,
                        Box::new(CallbackWriter::new(on_output.as_ref())),
                        Box::new(CallbackWriter::new(on_output.as_ref())),
                    )
                    .await?
            } else {
                // Fallback: stream directly to stderr
                ctx.client
                    .exec_stream(
                        ctx.container_id,
                        &opts,
                        Box::new(io::stderr()),
                        Box::new(io::stderr()),
                    )
                    .await?
            }
        } else {
            ctx.client.exec_command(ctx.container_id, &opts).await?
        };

        check_exit_code(&result, phase, None)?;
    }
    Ok(())
}

/// Run named lifecycle commands in parallel, cancelling siblings on first failure.
///
/// Uses `try_join_all` so that when any command fails, remaining in-flight
/// commands are cancelled (their futures are dropped) per the spec requirement.
async fn run_parallel(
    ctx: &LifecycleContext<'_>,
    phase: &str,
    commands: Vec<(String, Vec<String>)>,
) -> Result<(), BackendError> {
    let mut futures = Vec::new();
    for (name, cmd) in commands {
        let user = ctx.user.map(String::from);
        let env = ctx.env.to_vec();
        let working_dir = ctx.working_dir.map(String::from);
        let phase = phase.to_string();
        let container_id = ctx.container_id.to_string();

        futures.push(async move {
            debug!("{phase} [{name}]: {}", cmd.join(" "));
            let result = ctx
                .client
                .exec_command(
                    &container_id,
                    &ExecOptions {
                        cmd,
                        user,
                        env: Some(env),
                        working_dir,
                    },
                )
                .await?;

            check_exit_code(&result, &phase, Some(&name))?;
            Ok::<ExecResult, BackendError>(result)
        });
    }

    let results = futures_util::future::try_join_all(futures).await?;

    if ctx.is_text {
        print_completed_output(&results);
    }

    Ok(())
}

/// Check an exec result exit code, returning `LifecycleFailed` on non-zero.
fn check_exit_code(
    result: &ExecResult,
    phase: &str,
    name: Option<&str>,
) -> Result<(), BackendError> {
    if result.exit_code != 0 {
        let prefix = name.map_or(String::new(), |n| format!("[{n}] "));
        return Err(BackendError::LifecycleFailed {
            phase: phase.to_string(),
            message: format!(
                "{prefix}exit code {}: {}",
                result.exit_code,
                result.stderr.trim()
            ),
        });
    }
    Ok(())
}

/// Print stdout/stderr from completed parallel exec results to stderr.
fn print_completed_output(results: &[ExecResult]) {
    for exec_result in results {
        if !exec_result.stdout.is_empty() {
            eprint!("{}", exec_result.stdout);
        }
        if !exec_result.stderr.is_empty() {
            eprint!("{}", exec_result.stderr);
        }
    }
}

/// Execute lifecycle commands for a phase.
///
/// When `ctx.is_text` is true, prints origin-tracked progress (matching the original
/// devcontainer CLI phrasing) and streams sequential command output to stderr.
///
/// # Errors
///
/// Returns `BackendError::LifecycleFailed` if any command fails.
pub async fn run_lifecycle_phase(
    ctx: &LifecycleContext<'_>,
    phase: &str,
    value: &Value,
    origin: &str,
) -> Result<(), BackendError> {
    info!("Running the {phase} from {origin}...");
    debug!("Running {phase} from {origin}");

    match parse_lifecycle_command(value) {
        ParsedLifecycle::Sequential(commands) => run_sequential(ctx, phase, commands).await?,
        ParsedLifecycle::Parallel(commands) => run_parallel(ctx, phase, commands).await?,
    }

    debug!("{phase} completed");
    Ok(())
}

// ---------------------------------------------------------------------------
// Lifecycle phase management (merged from cella-orchestrator)
// ---------------------------------------------------------------------------

/// Shell-quote an argv array into a single command string safe for `sh -c`.
fn shell_quote_argv(argv: &[&str]) -> String {
    argv.iter()
        .map(|a| {
            if a.chars()
                .all(|c| c.is_alphanumeric() || "-_/.:=@".contains(c))
            {
                (*a).to_string()
            } else {
                format!("'{}'", a.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Tracks which lifecycle phases have already run inside a container.
///
/// Stored at `/tmp/.cella/lifecycle_state.json` so that restarts of prebuilt
/// containers can skip phases that already completed.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct LifecycleState {
    #[serde(default)]
    pub oncreate_done: bool,
}

/// Read the persisted lifecycle state from a running container.
pub async fn read_lifecycle_state(
    client: &dyn ContainerBackend,
    container_id: &str,
    user: &str,
) -> LifecycleState {
    let result = client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: vec![
                    "cat".to_string(),
                    "/tmp/.cella/lifecycle_state.json".to_string(),
                ],
                user: Some(user.to_string()),
                env: None,
                working_dir: None,
            },
        )
        .await;

    result
        .ok()
        .filter(|r| r.exit_code == 0)
        .and_then(|r| serde_json::from_str(r.stdout.trim()).ok())
        .unwrap_or_default()
}

/// Persist the lifecycle state inside a running container.
pub async fn write_lifecycle_state(
    client: &dyn ContainerBackend,
    container_id: &str,
    user: &str,
    state: &LifecycleState,
) {
    let json = serde_json::to_string(state).unwrap_or_default();
    let _ = client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: vec![
                    "sh".to_string(),
                    "-c".to_string(),
                    format!(
                        "mkdir -p /tmp/.cella && printf '%s' '{json}' > /tmp/.cella/lifecycle_state.json"
                    ),
                ],
                user: Some(user.to_string()),
                env: None,
                working_dir: None,
            },
        )
        .await;
}

/// Compute a deterministic hash of lifecycle command entries for a given phase.
pub fn hash_lifecycle_entries(entries: &[cella_features::LifecycleEntry]) -> String {
    let mut hasher = Sha256::new();
    for entry in entries {
        hasher.update(entry.origin.as_bytes());
        hasher.update(b":");
        hasher.update(entry.command.to_string().as_bytes());
        hasher.update(b"\n");
    }
    hex::encode(hasher.finalize())
}

/// Resolve lifecycle entries for a phase from feature-resolved config.
pub fn resolve_phase_entries<'a>(
    resolved_features: Option<&'a cella_features::ResolvedFeatures>,
    phase: &str,
) -> &'a [cella_features::LifecycleEntry] {
    resolved_features.map_or(&[], |rf| match phase {
        "onCreateCommand" => &rf.container_config.lifecycle.on_create,
        "updateContentCommand" => &rf.container_config.lifecycle.update_content,
        "postCreateCommand" => &rf.container_config.lifecycle.post_create,
        "postStartCommand" => &rf.container_config.lifecycle.post_start,
        "postAttachCommand" => &rf.container_config.lifecycle.post_attach,
        _ => &[],
    })
}

/// Resolve lifecycle entries for a phase, supporting both feature-resolved
/// configs and the metadata label format used by existing containers.
pub fn lifecycle_entries_for_phase(
    metadata: Option<&str>,
    config: &Value,
    phase: &str,
) -> Vec<cella_features::LifecycleEntry> {
    metadata.map_or_else(
        || {
            config
                .get(phase)
                .filter(|v| !v.is_null())
                .map(|cmd| {
                    vec![cella_features::LifecycleEntry {
                        origin: "devcontainer.json".into(),
                        command: cmd.clone(),
                    }]
                })
                .unwrap_or_default()
        },
        |meta_json| cella_features::lifecycle_from_metadata_label(meta_json, phase),
    )
}

/// Run a devcontainer.json config phase with progress output.
///
/// # Errors
///
/// Returns an error if the lifecycle command fails.
pub async fn run_config_phase_with_output(
    lc_ctx: &LifecycleContext<'_>,
    phase: &str,
    cmd: &Value,
    progress: &ProgressSender,
) -> Result<(), BackendError> {
    let label = format!("Running the {phase} from devcontainer.json...");
    let start = std::time::Instant::now();
    progress.println(&format!("  \x1b[36m▸\x1b[0m {label}"));
    let result = run_lifecycle_phase(lc_ctx, phase, cmd, "devcontainer.json").await;
    let elapsed = format_elapsed(start.elapsed());
    match &result {
        Ok(()) => progress.println(&format!("  \x1b[32m✓\x1b[0m {label}{elapsed}")),
        Err(e) => progress.println(&format!("  \x1b[31m✗\x1b[0m {label}: {e}")),
    }
    result
}

/// Run a sequence of origin-tracked lifecycle entries with progress tracking.
///
/// # Errors
///
/// Returns an error if any lifecycle entry's command fails.
pub async fn run_lifecycle_entries(
    ctx: &LifecycleContext<'_>,
    phase: &str,
    entries: &[cella_features::LifecycleEntry],
    progress: &ProgressSender,
) -> Result<(), BackendError> {
    for entry in entries {
        let label = format!("Running the {phase} from {}...", entry.origin);
        let start = std::time::Instant::now();
        progress.println(&format!("  \x1b[36m▸\x1b[0m {label}"));
        let result = run_lifecycle_phase(ctx, phase, &entry.command, &entry.origin).await;
        let elapsed = format_elapsed(start.elapsed());
        match &result {
            Ok(()) => progress.println(&format!("  \x1b[32m✓\x1b[0m {label}{elapsed}")),
            Err(e) => progress.println(&format!("  \x1b[31m✗\x1b[0m {label}: {e}")),
        }
        result?;
    }
    Ok(())
}

/// Resolve lifecycle entries using three-tier priority:
///
/// 1. `resolved_features` entries (from local feature build) -- if non-empty
/// 2. Image metadata entries (from prebuilt image) -- fallback
/// 3. `config.get(phase)` entries (from `devcontainer.json`) -- final fallback
fn resolve_entries_with_metadata(
    resolved_features: Option<&cella_features::ResolvedFeatures>,
    image_metadata: Option<&str>,
    config: &Value,
    phase: &str,
) -> Vec<cella_features::LifecycleEntry> {
    let feature_entries = resolve_phase_entries(resolved_features, phase);
    if !feature_entries.is_empty() {
        return feature_entries.to_vec();
    }

    if let Some(meta) = image_metadata {
        let meta_entries = cella_features::lifecycle_from_metadata_label(meta, phase);
        if !meta_entries.is_empty() {
            return meta_entries;
        }
    }

    config
        .get(phase)
        .filter(|v| !v.is_null())
        .map(|cmd| {
            vec![cella_features::LifecycleEntry {
                origin: "devcontainer.json".into(),
                command: cmd.clone(),
            }]
        })
        .unwrap_or_default()
}

/// Run all lifecycle phases for a first-create scenario.
///
/// # Errors
///
/// Returns an error if any lifecycle command fails.
pub async fn run_all_lifecycle_phases(
    lc_ctx: &LifecycleContext<'_>,
    config: &Value,
    resolved_features: Option<&cella_features::ResolvedFeatures>,
    image_metadata: Option<&str>,
    progress: &ProgressSender,
    post_resolve: Option<&PostResolveFn>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let phases = [
        "onCreateCommand",
        "updateContentCommand",
        "postCreateCommand",
        "postStartCommand",
        "postAttachCommand",
    ];

    for phase in phases {
        let mut entries =
            resolve_entries_with_metadata(resolved_features, image_metadata, config, phase);
        if let Some(f) = post_resolve {
            f(&mut entries);
        }
        run_lifecycle_entries(lc_ctx, phase, &entries, progress).await?;
    }

    Ok(())
}

/// Store the workspace content hash inside the container for future change detection.
pub async fn write_content_hash(
    client: &dyn ContainerBackend,
    container_id: &str,
    user: &str,
    workspace_root: &std::path::Path,
) {
    let hash = cella_git::content_hash::compute(workspace_root);
    let _ = client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: vec![
                    "sh".to_string(),
                    "-c".to_string(),
                    format!(
                        "mkdir -p /tmp/.cella && printf '%s' '{hash}' > /tmp/.cella/content_hash"
                    ),
                ],
                user: Some(user.to_string()),
                env: None,
                working_dir: None,
            },
        )
        .await;
}

/// Which lifecycle phase to wait for before returning from `cella up`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitForPhase {
    Initialize,
    OnCreate,
    UpdateContent,
    PostCreate,
    PostStart,
}

impl WaitForPhase {
    /// Parse from the `waitFor` config property. Default: `UpdateContentCommand`.
    pub fn from_config(config: &Value) -> Self {
        match config.get("waitFor").and_then(|v| v.as_str()) {
            Some("initializeCommand") => Self::Initialize,
            Some("onCreateCommand") => Self::OnCreate,
            Some("postCreateCommand") => Self::PostCreate,
            Some("postStartCommand") => Self::PostStart,
            _ => Self::UpdateContent,
        }
    }

    pub(crate) const fn ordinal(self) -> usize {
        match self {
            Self::Initialize => 0,
            Self::OnCreate => 1,
            Self::UpdateContent => 2,
            Self::PostCreate => 3,
            Self::PostStart => 4,
        }
    }
}

/// Run lifecycle phases up to `wait_for`, then spawn remaining phases in background.
///
/// # Errors
///
/// Returns an error if any foreground lifecycle command fails.
pub async fn run_lifecycle_phases_with_wait_for(
    lc_ctx: &LifecycleContext<'_>,
    config: &Value,
    resolved_features: Option<&cella_features::ResolvedFeatures>,
    image_metadata: Option<&str>,
    progress: &ProgressSender,
    wait_for: WaitForPhase,
    post_resolve: Option<&PostResolveFn>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let phases: &[&str] = &[
        "onCreateCommand",
        "updateContentCommand",
        "postCreateCommand",
        "postStartCommand",
        "postAttachCommand",
    ];

    let wait_index = match wait_for {
        WaitForPhase::Initialize => 0,
        _ => wait_for.ordinal(),
    };

    let mut background_cmds: Vec<String> = Vec::new();

    for (i, &phase) in phases.iter().enumerate() {
        let is_foreground = i < wait_index;
        let mut entries =
            resolve_entries_with_metadata(resolved_features, image_metadata, config, phase);
        if let Some(f) = post_resolve {
            f(&mut entries);
        }

        if is_foreground {
            run_lifecycle_entries(lc_ctx, phase, &entries, progress).await?;
        } else {
            for entry in &entries {
                background_cmds.push(entry_to_shell_command(entry));
            }
        }
    }

    if !background_cmds.is_empty() {
        let script = background_cmds.join("\n");
        let full_script = format!(
            "mkdir -p /tmp/.cella\n\
             (\n  set -e\n  {script}\n)\n\
             if [ $? -eq 0 ]; then\n\
               printf '{{\"status\":\"completed\"}}' > /tmp/.cella/lifecycle_status.json\n\
               printf '{{\"oncreate_done\":true}}' > /tmp/.cella/lifecycle_state.json\n\
             else\n\
               printf '{{\"status\":\"failed\"}}' > /tmp/.cella/lifecycle_status.json\n\
             fi\n"
        );

        debug!(
            "Spawning background lifecycle: {} commands",
            background_cmds.len()
        );
        let _ = lc_ctx
            .client
            .exec_detached(
                lc_ctx.container_id,
                &ExecOptions {
                    cmd: vec!["sh".to_string(), "-c".to_string(), full_script],
                    user: lc_ctx.user.map(String::from),
                    env: Some(lc_ctx.env.to_vec()),
                    working_dir: lc_ctx.working_dir.map(String::from),
                },
            )
            .await;
    }

    Ok(())
}

/// Convert a lifecycle entry into a shell command string for background execution.
fn entry_to_shell_command(entry: &cella_features::LifecycleEntry) -> String {
    if let Some(s) = entry.command.as_str() {
        return s.to_string();
    }
    if let Some(arr) = entry.command.as_array() {
        let cmd: Vec<&str> = arr.iter().filter_map(|v| v.as_str()).collect();
        if !cmd.is_empty() {
            return shell_quote_argv(&cmd);
        }
        return "true".to_string();
    }
    if let Some(obj) = entry.command.as_object() {
        let mut parallel = Vec::new();
        for val in obj.values() {
            if let Some(s) = val.as_str() {
                parallel.push(s.to_string());
            } else if let Some(arr) = val.as_array() {
                let cmd: Vec<&str> = arr.iter().filter_map(|v| v.as_str()).collect();
                if !cmd.is_empty() {
                    parallel.push(shell_quote_argv(&cmd));
                }
            }
        }
        if parallel.is_empty() {
            return "true".to_string();
        }
        let mut lines = Vec::new();
        lines.push("_cella_pids=\"\"".to_string());
        for cmd in &parallel {
            lines.push(format!("( {cmd} ) & _cella_pids=\"$_cella_pids $!\""));
        }
        lines.push("_cella_rc=0".to_string());
        lines.push(
            "for _cella_pid in $_cella_pids; do wait \"$_cella_pid\" || _cella_rc=1; done"
                .to_string(),
        );
        lines.push("[ \"$_cella_rc\" -eq 0 ] || exit 1".to_string());
        return lines.join("\n");
    }
    "true".to_string()
}

/// Check for workspace content changes and re-run updateContentCommand + postCreateCommand.
///
/// # Errors
///
/// Returns an error if any re-run lifecycle command fails.
pub async fn check_and_run_content_update(
    lc_ctx: &LifecycleContext<'_>,
    config: &Value,
    metadata: Option<&str>,
    workspace_root: &std::path::Path,
    progress: &ProgressSender,
    post_resolve: Option<&PostResolveFn>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let current_hash = cella_git::content_hash::compute(workspace_root);

    let read_result = lc_ctx
        .client
        .exec_command(
            lc_ctx.container_id,
            &ExecOptions {
                cmd: vec!["cat".to_string(), "/tmp/.cella/content_hash".to_string()],
                user: lc_ctx.user.map(String::from),
                env: None,
                working_dir: None,
            },
        )
        .await;

    let stored_hash = read_result
        .ok()
        .filter(|r| r.exit_code == 0)
        .map(|r| r.stdout.trim().to_string());

    if stored_hash.as_deref() == Some(&current_hash) {
        return Ok(());
    }

    progress.println("  Content changed, re-running updateContentCommand + postCreateCommand...");

    for phase in ["updateContentCommand", "postCreateCommand"] {
        let mut entries = lifecycle_entries_for_phase(metadata, config, phase);
        if let Some(f) = post_resolve {
            f(&mut entries);
        }
        run_lifecycle_entries(lc_ctx, phase, &entries, progress).await?;

        if entries.is_empty()
            && let Some(cmd) = config.get(phase)
            && !cmd.is_null()
        {
            run_config_phase_with_output(lc_ctx, phase, cmd, progress).await?;
        }
    }

    let _ = lc_ctx
        .client
        .exec_command(
            lc_ctx.container_id,
            &ExecOptions {
                cmd: vec![
                    "sh".to_string(),
                    "-c".to_string(),
                    format!("mkdir -p /tmp/.cella && printf '%s' '{current_hash}' > /tmp/.cella/content_hash"),
                ],
                user: lc_ctx.user.map(String::from),
                env: None,
                working_dir: None,
            },
        )
        .await;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_string_command() {
        let value = json!("echo hello");
        match parse_lifecycle_command(&value) {
            ParsedLifecycle::Sequential(cmds) => {
                assert_eq!(cmds.len(), 1);
                assert_eq!(cmds[0], vec!["sh", "-c", "echo hello"]);
            }
            ParsedLifecycle::Parallel(_) => panic!("expected Sequential"),
        }
    }

    #[test]
    fn parse_array_command() {
        let value = json!(["echo", "hello"]);
        match parse_lifecycle_command(&value) {
            ParsedLifecycle::Sequential(cmds) => {
                assert_eq!(cmds.len(), 1);
                assert_eq!(cmds[0], vec!["echo", "hello"]);
            }
            ParsedLifecycle::Parallel(_) => panic!("expected Sequential"),
        }
    }

    #[test]
    fn parse_object_commands() {
        let value = json!({"setup": "echo setup", "install": ["npm", "i"]});
        match parse_lifecycle_command(&value) {
            ParsedLifecycle::Parallel(cmds) => {
                assert_eq!(cmds.len(), 2);
            }
            ParsedLifecycle::Sequential(_) => panic!("expected Parallel"),
        }
    }

    #[test]
    fn parse_null_value() {
        let value = json!(null);
        match parse_lifecycle_command(&value) {
            ParsedLifecycle::Sequential(cmds) => {
                assert!(cmds.is_empty());
            }
            ParsedLifecycle::Parallel(_) => panic!("expected Sequential"),
        }
    }

    fn collect_callback_lines(input: &[u8]) -> Vec<String> {
        use std::sync::Mutex;

        let collected = Mutex::new(Vec::new());
        let callback = |line: &str| {
            collected.lock().unwrap().push(line.to_string());
        };

        let mut writer = CallbackWriter::new(&callback);
        io::Write::write_all(&mut writer, input).unwrap();
        drop(writer);

        collected.into_inner().unwrap()
    }

    #[test]
    fn callback_writer_indents_lines() {
        let lines = collect_callback_lines(b"first line\nsecond line\n");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "      first line");
        assert_eq!(lines[1], "      second line");
    }

    #[test]
    fn callback_writer_flushes_partial_line_on_drop() {
        let lines = collect_callback_lines(b"no newline");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0], "      no newline");
    }

    #[test]
    fn callback_writer_skips_blank_lines() {
        let lines = collect_callback_lines(b"content\n\n  \nanother\n");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "      content");
        assert_eq!(lines[1], "      another");
    }

    #[test]
    fn spec_string_command_wrapped_in_sh() {
        let value = json!("echo hello && echo world");
        match parse_lifecycle_command(&value) {
            ParsedLifecycle::Sequential(cmds) => {
                assert_eq!(cmds.len(), 1);
                assert_eq!(cmds[0][0], "sh");
                assert_eq!(cmds[0][1], "-c");
                assert_eq!(cmds[0][2], "echo hello && echo world");
            }
            ParsedLifecycle::Parallel(_) => panic!("expected Sequential"),
        }
    }

    #[test]
    fn spec_array_command_executed_directly() {
        let value = json!(["echo", "hello", "world"]);
        match parse_lifecycle_command(&value) {
            ParsedLifecycle::Sequential(cmds) => {
                assert_eq!(cmds.len(), 1);
                assert_eq!(cmds[0], vec!["echo", "hello", "world"]);
            }
            ParsedLifecycle::Parallel(_) => panic!("expected Sequential"),
        }
    }

    #[test]
    fn spec_object_command_is_parallel() {
        let value = json!({"setup": "npm install", "db": ["mysql", "-u", "root"]});
        match parse_lifecycle_command(&value) {
            ParsedLifecycle::Parallel(cmds) => {
                assert_eq!(cmds.len(), 2);
                let setup = cmds.iter().find(|(n, _)| n == "setup").unwrap();
                assert_eq!(setup.1, vec!["sh", "-c", "npm install"]);
                let db = cmds.iter().find(|(n, _)| n == "db").unwrap();
                assert_eq!(db.1, vec!["mysql", "-u", "root"]);
            }
            ParsedLifecycle::Sequential(_) => panic!("expected Parallel"),
        }
    }

    #[test]
    fn spec_object_string_values_wrapped_in_sh() {
        let value = json!({"server": "npm start", "client": "npm run dev"});
        match parse_lifecycle_command(&value) {
            ParsedLifecycle::Parallel(cmds) => {
                for (_, cmd) in &cmds {
                    assert_eq!(cmd[0], "sh");
                    assert_eq!(cmd[1], "-c");
                }
            }
            ParsedLifecycle::Sequential(_) => panic!("expected Parallel"),
        }
    }

    #[test]
    fn spec_empty_string_is_valid_command() {
        let value = json!("");
        match parse_lifecycle_command(&value) {
            ParsedLifecycle::Sequential(cmds) => {
                assert_eq!(cmds.len(), 1);
                assert_eq!(cmds[0], vec!["sh", "-c", ""]);
            }
            ParsedLifecycle::Parallel(_) => panic!("expected Sequential"),
        }
    }

    #[test]
    fn spec_empty_object_no_parallel_commands() {
        let value = json!({});
        match parse_lifecycle_command(&value) {
            ParsedLifecycle::Parallel(cmds) => assert!(cmds.is_empty()),
            ParsedLifecycle::Sequential(_) => panic!("expected Parallel"),
        }
    }

    #[test]
    fn spec_lifecycle_phase_order() {
        let phases = [
            "initializeCommand",
            "onCreateCommand",
            "updateContentCommand",
            "postCreateCommand",
            "postStartCommand",
            "postAttachCommand",
        ];
        for i in 0..phases.len() - 1 {
            assert!(
                phases.iter().position(|p| *p == phases[i]).unwrap()
                    < phases.iter().position(|p| *p == phases[i + 1]).unwrap()
            );
        }
    }

    #[test]
    fn spec_resume_only_post_start_and_attach() {
        let resume_phases = ["postStartCommand", "postAttachCommand"];
        let creation_only = [
            "initializeCommand",
            "onCreateCommand",
            "updateContentCommand",
            "postCreateCommand",
        ];
        for phase in &creation_only {
            assert!(!resume_phases.contains(phase));
        }
    }

    #[test]
    fn check_exit_code_zero_is_ok() {
        let result = ExecResult {
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
        };
        assert!(check_exit_code(&result, "postCreateCommand", None).is_ok());
    }

    #[test]
    fn check_exit_code_nonzero_returns_error() {
        let result = ExecResult {
            exit_code: 1,
            stdout: String::new(),
            stderr: "command not found".to_string(),
        };
        let err = check_exit_code(&result, "onCreateCommand", None).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("onCreateCommand"),
            "error should contain phase name, got: {msg}"
        );
    }

    #[test]
    fn check_exit_code_nonzero_includes_exit_code_in_message() {
        let result = ExecResult {
            exit_code: 127,
            stdout: String::new(),
            stderr: "sh: npm: not found".to_string(),
        };
        let err = check_exit_code(&result, "postCreateCommand", None).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("127"),
            "error should contain exit code, got: {msg}"
        );
    }

    #[test]
    fn check_exit_code_nonzero_includes_stderr() {
        let result = ExecResult {
            exit_code: 2,
            stdout: "some output\n".to_string(),
            stderr: "fatal error occurred\n".to_string(),
        };
        let err = check_exit_code(&result, "updateContentCommand", None).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("fatal error occurred"),
            "error should contain trimmed stderr, got: {msg}"
        );
    }

    #[test]
    fn check_exit_code_with_named_prefix() {
        let result = ExecResult {
            exit_code: 1,
            stdout: String::new(),
            stderr: "failed".to_string(),
        };
        let err = check_exit_code(&result, "postStartCommand", Some("setup")).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("[setup]"),
            "error should contain [name] prefix, got: {msg}"
        );
    }

    #[test]
    fn check_exit_code_named_prefix_absent_when_none() {
        let result = ExecResult {
            exit_code: 1,
            stdout: String::new(),
            stderr: "err".to_string(),
        };
        let err = check_exit_code(&result, "phase", None).unwrap_err();
        let msg = format!("{err}");
        assert!(
            !msg.contains('['),
            "no bracket prefix expected without name, got: {msg}"
        );
    }

    #[test]
    fn check_exit_code_zero_with_name_still_ok() {
        let result = ExecResult {
            exit_code: 0,
            stdout: "done".to_string(),
            stderr: String::new(),
        };
        assert!(check_exit_code(&result, "phase", Some("task")).is_ok());
    }

    #[test]
    fn check_exit_code_stderr_trimmed() {
        let result = ExecResult {
            exit_code: 1,
            stdout: String::new(),
            stderr: "  whitespace  \n".to_string(),
        };
        let err = check_exit_code(&result, "phase", None).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("whitespace"),
            "expected trimmed stderr in message, got: {msg}"
        );
        assert!(!msg.ends_with('\n'), "stderr should be trimmed, got: {msg}");
    }

    #[test]
    fn parse_object_with_non_string_non_array_value() {
        let value = json!({"check": 42});
        match parse_lifecycle_command(&value) {
            ParsedLifecycle::Parallel(cmds) => {
                assert_eq!(cmds.len(), 1);
                assert_eq!(cmds[0].1[0], "sh");
                assert_eq!(cmds[0].1[1], "-c");
                assert_eq!(cmds[0].1[2], "42");
            }
            ParsedLifecycle::Sequential(_) => panic!("expected Parallel"),
        }
    }

    #[test]
    fn parse_boolean_value() {
        let value = json!(true);
        match parse_lifecycle_command(&value) {
            ParsedLifecycle::Sequential(cmds) => assert!(cmds.is_empty()),
            ParsedLifecycle::Parallel(_) => panic!("expected Sequential"),
        }
    }

    #[test]
    fn parse_number_value() {
        let value = json!(42);
        match parse_lifecycle_command(&value) {
            ParsedLifecycle::Sequential(cmds) => assert!(cmds.is_empty()),
            ParsedLifecycle::Parallel(_) => panic!("expected Sequential"),
        }
    }

    #[test]
    fn parse_array_filters_non_string_elements() {
        let value = json!(["echo", 42, "hello", null]);
        match parse_lifecycle_command(&value) {
            ParsedLifecycle::Sequential(cmds) => {
                assert_eq!(cmds.len(), 1);
                assert_eq!(cmds[0], vec!["echo", "hello"]);
            }
            ParsedLifecycle::Parallel(_) => panic!("expected Sequential"),
        }
    }

    #[test]
    fn parse_empty_array() {
        let value = json!([]);
        match parse_lifecycle_command(&value) {
            ParsedLifecycle::Sequential(cmds) => {
                assert_eq!(cmds.len(), 1);
                assert!(cmds[0].is_empty());
            }
            ParsedLifecycle::Parallel(_) => panic!("expected Sequential"),
        }
    }

    #[test]
    fn callback_writer_handles_multiple_writes_for_one_line() {
        use std::sync::Mutex;

        let collected = Mutex::new(Vec::new());
        let callback = |line: &str| {
            collected.lock().unwrap().push(line.to_string());
        };

        let mut writer = CallbackWriter::new(&callback);
        io::Write::write_all(&mut writer, b"hello ").unwrap();
        io::Write::write_all(&mut writer, b"world\n").unwrap();
        drop(writer);

        let lines = collected.into_inner().unwrap();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0], "      hello world");
    }

    #[test]
    fn callback_writer_handles_empty_input() {
        let lines = collect_callback_lines(b"");
        assert!(lines.is_empty());
    }

    #[test]
    fn callback_writer_only_newlines() {
        let lines = collect_callback_lines(b"\n\n\n");
        assert!(lines.is_empty());
    }

    // ── Lifecycle phase management tests (from orchestrator) ─────────────

    fn make_resolved_features(
        lifecycle: cella_features::FeatureLifecycle,
    ) -> cella_features::ResolvedFeatures {
        cella_features::ResolvedFeatures {
            features: vec![],
            dockerfile: String::new(),
            build_context: std::path::PathBuf::from("/tmp"),
            container_config: cella_features::FeatureContainerConfig {
                lifecycle,
                ..Default::default()
            },
            metadata_label: String::new(),
        }
    }

    #[test]
    fn resolve_phase_entries_on_create() {
        let mut lifecycle = cella_features::FeatureLifecycle::default();
        lifecycle.on_create.push(cella_features::LifecycleEntry {
            origin: "test-feature".into(),
            command: json!("echo hello"),
        });
        let rf = make_resolved_features(lifecycle);
        let entries = resolve_phase_entries(Some(&rf), "onCreateCommand");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].origin, "test-feature");
        assert_eq!(entries[0].command, json!("echo hello"));
    }

    #[test]
    fn resolve_phase_entries_all_phases() {
        let mut lifecycle = cella_features::FeatureLifecycle::default();
        lifecycle.on_create.push(cella_features::LifecycleEntry {
            origin: "a".into(),
            command: json!("1"),
        });
        lifecycle
            .update_content
            .push(cella_features::LifecycleEntry {
                origin: "b".into(),
                command: json!("2"),
            });
        lifecycle.post_create.push(cella_features::LifecycleEntry {
            origin: "c".into(),
            command: json!("3"),
        });
        lifecycle.post_start.push(cella_features::LifecycleEntry {
            origin: "d".into(),
            command: json!("4"),
        });
        lifecycle.post_attach.push(cella_features::LifecycleEntry {
            origin: "e".into(),
            command: json!("5"),
        });
        let rf = make_resolved_features(lifecycle);
        assert_eq!(resolve_phase_entries(Some(&rf), "onCreateCommand").len(), 1);
        assert_eq!(
            resolve_phase_entries(Some(&rf), "updateContentCommand").len(),
            1
        );
        assert_eq!(
            resolve_phase_entries(Some(&rf), "postCreateCommand").len(),
            1
        );
        assert_eq!(
            resolve_phase_entries(Some(&rf), "postStartCommand").len(),
            1
        );
        assert_eq!(
            resolve_phase_entries(Some(&rf), "postAttachCommand").len(),
            1
        );
    }

    #[test]
    fn resolve_phase_entries_unknown_phase_returns_empty() {
        let rf = make_resolved_features(cella_features::FeatureLifecycle::default());
        let entries = resolve_phase_entries(Some(&rf), "nonExistentPhase");
        assert!(entries.is_empty());
    }

    #[test]
    fn resolve_phase_entries_none_features_returns_empty() {
        let entries = resolve_phase_entries(None, "onCreateCommand");
        assert!(entries.is_empty());
    }

    #[test]
    fn lifecycle_entries_for_phase_from_config_string() {
        let config = json!({"postCreateCommand": "npm install"});
        let entries = lifecycle_entries_for_phase(None, &config, "postCreateCommand");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].origin, "devcontainer.json");
        assert_eq!(entries[0].command, json!("npm install"));
    }

    #[test]
    fn lifecycle_entries_for_phase_from_config_null() {
        let config = json!({"postCreateCommand": null});
        let entries = lifecycle_entries_for_phase(None, &config, "postCreateCommand");
        assert!(entries.is_empty());
    }

    #[test]
    fn lifecycle_entries_for_phase_missing_key() {
        let config = json!({});
        let entries = lifecycle_entries_for_phase(None, &config, "postCreateCommand");
        assert!(entries.is_empty());
    }

    #[test]
    fn lifecycle_entries_wait_for_phase_values() {
        assert_eq!(
            WaitForPhase::from_config(&json!({"waitFor": "initializeCommand"})),
            WaitForPhase::Initialize
        );
        assert_eq!(
            WaitForPhase::from_config(&json!({"waitFor": "onCreateCommand"})),
            WaitForPhase::OnCreate
        );
        assert_eq!(
            WaitForPhase::from_config(&json!({"waitFor": "postCreateCommand"})),
            WaitForPhase::PostCreate
        );
        assert_eq!(
            WaitForPhase::from_config(&json!({"waitFor": "postStartCommand"})),
            WaitForPhase::PostStart
        );
        assert_eq!(
            WaitForPhase::from_config(&json!({})),
            WaitForPhase::UpdateContent
        );
        assert_eq!(
            WaitForPhase::from_config(&json!({"waitFor": "updateContentCommand"})),
            WaitForPhase::UpdateContent
        );
    }

    #[test]
    fn lifecycle_entries_for_phase_from_config_array() {
        let config = json!({"postCreateCommand": ["npm", "install"]});
        let entries = lifecycle_entries_for_phase(None, &config, "postCreateCommand");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].command, json!(["npm", "install"]));
    }

    #[test]
    fn lifecycle_entries_for_phase_from_config_object() {
        let config = json!({
            "postCreateCommand": {
                "build": "npm run build",
                "test": "npm test"
            }
        });
        let entries = lifecycle_entries_for_phase(None, &config, "postCreateCommand");
        assert_eq!(entries.len(), 1);
        assert!(entries[0].command.is_object());
    }

    #[test]
    fn lifecycle_entries_for_phase_from_metadata_label() {
        let metadata = r#"[{"id":"ghcr.io/features/node","postCreateCommand":"npm i"}]"#;
        let entries = lifecycle_entries_for_phase(Some(metadata), &json!({}), "postCreateCommand");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].origin, "ghcr.io/features/node");
        assert_eq!(entries[0].command, json!("npm i"));
    }

    #[test]
    fn lifecycle_entries_for_phase_metadata_missing_phase() {
        let metadata = r#"[{"id":"feature-a","onCreateCommand":"echo hi"}]"#;
        let entries = lifecycle_entries_for_phase(Some(metadata), &json!({}), "postCreateCommand");
        assert!(entries.is_empty());
    }

    #[test]
    fn lifecycle_entries_for_phase_metadata_null_command_skipped() {
        let metadata = r#"[{"id":"feature-a","postCreateCommand":null}]"#;
        let entries = lifecycle_entries_for_phase(Some(metadata), &json!({}), "postCreateCommand");
        assert!(entries.is_empty());
    }

    #[test]
    fn lifecycle_entries_for_phase_metadata_multiple_entries() {
        let metadata = r#"[
            {"id":"feat-a","postStartCommand":"echo a"},
            {"id":"feat-b","postStartCommand":"echo b"}
        ]"#;
        let entries = lifecycle_entries_for_phase(Some(metadata), &json!({}), "postStartCommand");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].origin, "feat-a");
        assert_eq!(entries[1].origin, "feat-b");
    }

    #[test]
    fn lifecycle_entries_for_phase_metadata_no_id_defaults_to_devcontainer_json() {
        let metadata = r#"[{"postCreateCommand":"echo hi"}]"#;
        let entries = lifecycle_entries_for_phase(Some(metadata), &json!({}), "postCreateCommand");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].origin, "devcontainer.json");
    }

    #[test]
    fn lifecycle_entries_for_phase_invalid_metadata_returns_empty() {
        let metadata = "not valid json";
        let entries = lifecycle_entries_for_phase(Some(metadata), &json!({}), "postCreateCommand");
        assert!(entries.is_empty());
    }

    #[test]
    fn resolve_phase_entries_multiple_entries_per_phase() {
        let mut lifecycle = cella_features::FeatureLifecycle::default();
        lifecycle.on_create.push(cella_features::LifecycleEntry {
            origin: "feat-a".into(),
            command: json!("echo a"),
        });
        lifecycle.on_create.push(cella_features::LifecycleEntry {
            origin: "feat-b".into(),
            command: json!(["npm", "install"]),
        });
        let rf = make_resolved_features(lifecycle);
        let entries = resolve_phase_entries(Some(&rf), "onCreateCommand");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].command, json!("echo a"));
        assert_eq!(entries[1].command, json!(["npm", "install"]));
    }

    #[test]
    fn resolve_phase_entries_empty_lifecycle() {
        let rf = make_resolved_features(cella_features::FeatureLifecycle::default());
        for phase in &[
            "onCreateCommand",
            "updateContentCommand",
            "postCreateCommand",
            "postStartCommand",
            "postAttachCommand",
        ] {
            assert!(resolve_phase_entries(Some(&rf), phase).is_empty());
        }
    }

    #[test]
    fn wait_for_phase_ordinal_ordering() {
        assert!(WaitForPhase::Initialize.ordinal() < WaitForPhase::OnCreate.ordinal());
        assert!(WaitForPhase::OnCreate.ordinal() < WaitForPhase::UpdateContent.ordinal());
        assert!(WaitForPhase::UpdateContent.ordinal() < WaitForPhase::PostCreate.ordinal());
        assert!(WaitForPhase::PostCreate.ordinal() < WaitForPhase::PostStart.ordinal());
    }

    #[test]
    fn wait_for_phase_unknown_string_defaults_to_update_content() {
        assert_eq!(
            WaitForPhase::from_config(&json!({"waitFor": "somethingBogus"})),
            WaitForPhase::UpdateContent
        );
    }

    #[test]
    fn wait_for_phase_null_value_defaults_to_update_content() {
        assert_eq!(
            WaitForPhase::from_config(&json!({"waitFor": null})),
            WaitForPhase::UpdateContent
        );
    }

    #[test]
    fn lifecycle_state_default_values() {
        let state = LifecycleState::default();
        assert!(!state.oncreate_done);
    }

    #[test]
    fn lifecycle_state_roundtrip_serde() {
        let state = LifecycleState {
            oncreate_done: true,
        };
        let json = serde_json::to_string(&state).unwrap();
        let deserialized: LifecycleState = serde_json::from_str(&json).unwrap();
        assert!(deserialized.oncreate_done);
    }

    #[test]
    fn lifecycle_state_deserialize_empty_object() {
        let state: LifecycleState = serde_json::from_str("{}").unwrap();
        assert!(!state.oncreate_done);
    }

    #[test]
    fn lifecycle_state_deserialize_partial() {
        let state: LifecycleState = serde_json::from_str(r#"{"oncreate_done": true}"#).unwrap();
        assert!(state.oncreate_done);
    }

    #[test]
    fn hash_lifecycle_entries_deterministic() {
        let entries = vec![cella_features::LifecycleEntry {
            origin: "feature-a".into(),
            command: json!("npm install"),
        }];
        let h1 = hash_lifecycle_entries(&entries);
        let h2 = hash_lifecycle_entries(&entries);
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64);
    }

    #[test]
    fn hash_lifecycle_entries_different_for_different_entries() {
        let entries_a = vec![cella_features::LifecycleEntry {
            origin: "a".into(),
            command: json!("echo a"),
        }];
        let entries_b = vec![cella_features::LifecycleEntry {
            origin: "b".into(),
            command: json!("echo b"),
        }];
        assert_ne!(
            hash_lifecycle_entries(&entries_a),
            hash_lifecycle_entries(&entries_b)
        );
    }

    #[test]
    fn hash_lifecycle_entries_empty() {
        let h = hash_lifecycle_entries(&[]);
        assert_eq!(h.len(), 64);
    }

    #[test]
    fn resolve_entries_tier1_features_take_priority() {
        let mut lifecycle = cella_features::FeatureLifecycle::default();
        lifecycle.on_create.push(cella_features::LifecycleEntry {
            origin: "feature-x".into(),
            command: json!("from features"),
        });
        let rf = make_resolved_features(lifecycle);
        let metadata = r#"[{"id":"img","onCreateCommand":"from metadata"}]"#;
        let config = json!({"onCreateCommand": "from config"});

        let entries =
            resolve_entries_with_metadata(Some(&rf), Some(metadata), &config, "onCreateCommand");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].origin, "feature-x");
    }

    #[test]
    fn resolve_entries_tier2_metadata_when_no_features() {
        let metadata = r#"[{"id":"prebuilt","onCreateCommand":"from metadata"}]"#;
        let config = json!({"onCreateCommand": "from config"});

        let entries =
            resolve_entries_with_metadata(None, Some(metadata), &config, "onCreateCommand");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].origin, "prebuilt");
        assert_eq!(entries[0].command, json!("from metadata"));
    }

    #[test]
    fn resolve_entries_tier3_config_when_no_features_or_metadata() {
        let config = json!({"onCreateCommand": "from config"});

        let entries = resolve_entries_with_metadata(None, None, &config, "onCreateCommand");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].origin, "devcontainer.json");
        assert_eq!(entries[0].command, json!("from config"));
    }

    #[test]
    fn resolve_entries_tier3_config_when_metadata_lacks_phase() {
        let metadata = r#"[{"id":"prebuilt","postStartCommand":"echo hi"}]"#;
        let config = json!({"onCreateCommand": "from config"});

        let entries =
            resolve_entries_with_metadata(None, Some(metadata), &config, "onCreateCommand");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].origin, "devcontainer.json");
    }

    #[test]
    fn resolve_entries_empty_when_nothing_defined() {
        let config = json!({});
        let entries = resolve_entries_with_metadata(None, None, &config, "onCreateCommand");
        assert!(entries.is_empty());
    }

    #[test]
    fn resolve_entries_empty_features_falls_through_to_metadata() {
        let rf = make_resolved_features(cella_features::FeatureLifecycle::default());
        let metadata = r#"[{"id":"prebuilt","postCreateCommand":"setup"}]"#;
        let config = json!({});

        let entries =
            resolve_entries_with_metadata(Some(&rf), Some(metadata), &config, "postCreateCommand");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].origin, "prebuilt");
    }

    #[test]
    fn entry_to_shell_command_string() {
        let entry = cella_features::LifecycleEntry {
            origin: "test".into(),
            command: json!("npm install"),
        };
        assert_eq!(entry_to_shell_command(&entry), "npm install");
    }

    #[test]
    fn entry_to_shell_command_array() {
        let entry = cella_features::LifecycleEntry {
            origin: "test".into(),
            command: json!(["echo", "hello world"]),
        };
        assert_eq!(entry_to_shell_command(&entry), "echo 'hello world'");
    }

    #[test]
    fn entry_to_shell_command_object_tracks_pids() {
        let entry = cella_features::LifecycleEntry {
            origin: "test".into(),
            command: json!({"build": "npm run build", "lint": "npm run lint"}),
        };
        let cmd = entry_to_shell_command(&entry);
        assert!(cmd.contains("_cella_pids"));
        assert!(cmd.contains("wait"));
        assert!(cmd.contains("exit 1"));
    }

    #[test]
    fn entry_to_shell_command_null_is_noop() {
        let entry = cella_features::LifecycleEntry {
            origin: "test".into(),
            command: json!(null),
        };
        assert_eq!(entry_to_shell_command(&entry), "true");
    }
}
