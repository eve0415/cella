//! Lifecycle phase management: resolution, execution, and content tracking.

use tracing::debug;

use cella_docker::{
    CellaDockerError, DockerClient, ExecOptions, LifecycleContext, run_lifecycle_phase,
};

/// Resolve lifecycle entries for a phase from feature-resolved config.
pub(super) fn resolve_phase_entries<'a>(
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

/// Run a devcontainer.json config phase with progress output.
///
/// Used when no feature-based lifecycle entries exist for the phase but
/// devcontainer.json defines the command directly.
pub(super) async fn run_config_phase_with_output(
    lc_ctx: &LifecycleContext<'_>,
    phase: &str,
    cmd: &serde_json::Value,
    progress: &crate::progress::Progress,
) -> Result<(), CellaDockerError> {
    let label = format!("Running the {phase} from devcontainer.json...");
    let start = std::time::Instant::now();
    progress.println(&format!("  \x1b[36m▸\x1b[0m {label}"));
    let result = run_lifecycle_phase(lc_ctx, phase, cmd, "devcontainer.json").await;
    let elapsed = crate::progress::format_elapsed_pub(start.elapsed());
    match &result {
        Ok(()) => progress.println(&format!("  \x1b[32m✓\x1b[0m {label}{elapsed}")),
        Err(e) => progress.println(&format!("  \x1b[31m✗\x1b[0m {label}: {e}")),
    }
    result
}

/// Run a sequence of origin-tracked lifecycle entries with progress tracking.
///
/// Prints a permanent header line before each lifecycle command, then streams
/// the command's output indented below it, then prints the completion status.
pub(super) async fn run_lifecycle_entries(
    ctx: &LifecycleContext<'_>,
    phase: &str,
    entries: &[cella_features::LifecycleEntry],
    progress: &crate::progress::Progress,
) -> Result<(), CellaDockerError> {
    for entry in entries {
        let label = format!("Running the {phase} from {}...", entry.origin);
        let start = std::time::Instant::now();
        // Print header so user knows what's running during streaming.
        progress.println(&format!("  \x1b[36m▸\x1b[0m {label}"));
        let result = run_lifecycle_phase(ctx, phase, &entry.command, &entry.origin).await;
        let elapsed = crate::progress::format_elapsed_pub(start.elapsed());
        match &result {
            Ok(()) => progress.println(&format!("  \x1b[32m✓\x1b[0m {label}{elapsed}")),
            Err(e) => progress.println(&format!("  \x1b[31m✗\x1b[0m {label}: {e}")),
        }
        result?;
    }
    Ok(())
}

/// Run all lifecycle phases for a first-create scenario.
pub async fn run_all_lifecycle_phases(
    lc_ctx: &LifecycleContext<'_>,
    config: &serde_json::Value,
    resolved_features: Option<&cella_features::ResolvedFeatures>,
    progress: &crate::progress::Progress,
) -> Result<(), Box<dyn std::error::Error>> {
    let phases = [
        "onCreateCommand",
        "updateContentCommand",
        "postCreateCommand",
        "postStartCommand",
        "postAttachCommand",
    ];

    for phase in phases {
        let entries = resolve_phase_entries(resolved_features, phase);

        run_lifecycle_entries(lc_ctx, phase, entries, progress).await?;

        if entries.is_empty()
            && let Some(cmd) = config.get(phase)
            && !cmd.is_null()
        {
            run_config_phase_with_output(lc_ctx, phase, cmd, progress).await?;
        }
    }

    Ok(())
}

/// Store the workspace content hash inside the container for future change detection.
pub(super) async fn write_content_hash(
    client: &DockerClient,
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
    pub fn from_config(config: &serde_json::Value) -> Self {
        match config.get("waitFor").and_then(|v| v.as_str()) {
            Some("initializeCommand") => Self::Initialize,
            Some("onCreateCommand") => Self::OnCreate,
            Some("postCreateCommand") => Self::PostCreate,
            Some("postStartCommand") => Self::PostStart,
            _ => Self::UpdateContent,
        }
    }

    /// Index into the in-container lifecycle phases array.
    const fn ordinal(self) -> usize {
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
/// Returns after the `wait_for` phase completes. Remaining phases run as a detached
/// exec inside the container.
pub async fn run_lifecycle_phases_with_wait_for(
    lc_ctx: &LifecycleContext<'_>,
    config: &serde_json::Value,
    resolved_features: Option<&cella_features::ResolvedFeatures>,
    progress: &crate::progress::Progress,
    wait_for: WaitForPhase,
) -> Result<(), Box<dyn std::error::Error>> {
    let phases: &[&str] = &[
        "onCreateCommand",
        "updateContentCommand",
        "postCreateCommand",
        "postStartCommand",
        "postAttachCommand",
    ];

    // initializeCommand (index 0) runs on host, not in these phases.
    // The ordinal maps to in-container phases starting at 1 for onCreateCommand.
    let wait_index = match wait_for {
        WaitForPhase::Initialize => 0,
        _ => wait_for.ordinal(),
    };

    let mut background_cmds: Vec<String> = Vec::new();

    for (i, &phase) in phases.iter().enumerate() {
        let is_foreground = i < wait_index;
        let entries = resolve_phase_entries(resolved_features, phase);

        if is_foreground {
            // Run synchronously
            run_lifecycle_entries(lc_ctx, phase, entries, progress).await?;

            if entries.is_empty()
                && let Some(cmd) = config.get(phase)
                && !cmd.is_null()
            {
                run_config_phase_with_output(lc_ctx, phase, cmd, progress).await?;
            }
        } else {
            // Collect for background execution
            for entry in entries {
                if let Some(s) = entry.command.as_str() {
                    background_cmds.push(s.to_string());
                } else if let Some(arr) = entry.command.as_array() {
                    let cmd: Vec<&str> = arr.iter().filter_map(|v| v.as_str()).collect();
                    if !cmd.is_empty() {
                        background_cmds.push(cmd.join(" "));
                    }
                }
            }
            if entries.is_empty()
                && let Some(cmd) = config.get(phase)
                && let Some(s) = cmd.as_str()
            {
                background_cmds.push(s.to_string());
            }
        }
    }

    // Spawn remaining phases in background
    if !background_cmds.is_empty() {
        let script = background_cmds.join("\n");
        let full_script = format!(
            "mkdir -p /tmp/.cella\n\
             {script}\n\
             printf '{{\"status\":\"completed\"}}' > /tmp/.cella/lifecycle_status.json\n"
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

/// Check for workspace content changes and re-run updateContentCommand + postCreateCommand.
///
/// Computes a content hash from the workspace, compares it to the stored hash
/// inside the container at `/tmp/.cella/content_hash`. If different, runs the
/// updateContentCommand and postCreateCommand phases, then writes the new hash.
pub(super) async fn check_and_run_content_update(
    lc_ctx: &LifecycleContext<'_>,
    config: &serde_json::Value,
    metadata: Option<&str>,
    workspace_root: &std::path::Path,
    progress: &crate::progress::Progress,
) -> Result<(), Box<dyn std::error::Error>> {
    let current_hash = cella_git::content_hash::compute(workspace_root);

    // Read stored hash from container
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
        let entries = super::lifecycle_entries_for_phase(metadata, config, phase);
        run_lifecycle_entries(lc_ctx, phase, &entries, progress).await?;

        if entries.is_empty()
            && let Some(cmd) = config.get(phase)
            && !cmd.is_null()
        {
            run_config_phase_with_output(lc_ctx, phase, cmd, progress).await?;
        }
    }

    // Write updated hash
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
