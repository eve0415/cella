//! Lifecycle phase management: resolution, execution, and content tracking.
//!
//! Moved from `cella-cli/src/commands/up/lifecycle.rs`. Uses
//! [`ProgressSender`] instead of the indicatif-coupled `Progress` type.

use sha2::{Digest, Sha256};
use tracing::debug;

use cella_backend::{
    BackendError, ContainerBackend, ExecOptions, LifecycleContext, run_lifecycle_phase,
};

use crate::progress::{ProgressSender, format_elapsed};

/// Shell-quote an argv array into a single command string safe for `sh -c`.
///
/// Arguments containing only safe characters (alphanumeric plus a small set of
/// punctuation) are passed through unquoted. Everything else is single-quoted
/// with embedded single-quotes escaped via the `'\''` idiom.
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

// ---------------------------------------------------------------------------
// Lifecycle state tracking (persisted inside the container)
// ---------------------------------------------------------------------------

/// Tracks which lifecycle phases have already run inside a container.
///
/// Stored at `/tmp/.cella/lifecycle_state.json` so that restarts of prebuilt
/// containers can skip phases that already completed.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct LifecycleState {
    /// Whether `onCreateCommand` has already been executed.
    #[serde(default)]
    pub oncreate_done: bool,
}

/// Read the persisted lifecycle state from a running container.
///
/// Returns [`LifecycleState::default()`] when the file does not exist or
/// cannot be parsed (e.g. first run on a fresh container).
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
///
/// Used to detect whether the commands associated with `updateContentCommand`
/// have changed between container restarts.
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
    config: &serde_json::Value,
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
/// Used when no feature-based lifecycle entries exist for the phase but
/// devcontainer.json defines the command directly.
///
/// # Errors
///
/// Returns an error if the lifecycle command fails.
pub async fn run_config_phase_with_output(
    lc_ctx: &LifecycleContext<'_>,
    phase: &str,
    cmd: &serde_json::Value,
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
/// Prints a permanent header line before each lifecycle command, then streams
/// the command's output indented below it, then prints the completion status.
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
    config: &serde_json::Value,
    phase: &str,
) -> Vec<cella_features::LifecycleEntry> {
    // Tier 1: feature-resolved entries
    let feature_entries = resolve_phase_entries(resolved_features, phase);
    if !feature_entries.is_empty() {
        return feature_entries.to_vec();
    }

    // Tier 2: image metadata entries (prebuilt image)
    if let Some(meta) = image_metadata {
        let meta_entries = cella_features::lifecycle_from_metadata_label(meta, phase);
        if !meta_entries.is_empty() {
            return meta_entries;
        }
    }

    // Tier 3: direct devcontainer.json config
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
    config: &serde_json::Value,
    resolved_features: Option<&cella_features::ResolvedFeatures>,
    image_metadata: Option<&str>,
    progress: &ProgressSender,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let phases = [
        "onCreateCommand",
        "updateContentCommand",
        "postCreateCommand",
        "postStartCommand",
        "postAttachCommand",
    ];

    for phase in phases {
        let entries =
            resolve_entries_with_metadata(resolved_features, image_metadata, config, phase);
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
/// Returns after the `wait_for` phase completes. Remaining phases run as a detached
/// exec inside the container. Failures in background phases are recorded in
/// `/tmp/.cella/lifecycle_status.json`.
///
/// # Errors
///
/// Returns an error if any foreground lifecycle command fails.
pub async fn run_lifecycle_phases_with_wait_for(
    lc_ctx: &LifecycleContext<'_>,
    config: &serde_json::Value,
    resolved_features: Option<&cella_features::ResolvedFeatures>,
    image_metadata: Option<&str>,
    progress: &ProgressSender,
    wait_for: WaitForPhase,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let phases: &[&str] = &[
        "onCreateCommand",
        "updateContentCommand",
        "postCreateCommand",
        "postStartCommand",
        "postAttachCommand",
    ];

    // initializeCommand (index 0) runs on host, not in these phases.
    let wait_index = match wait_for {
        WaitForPhase::Initialize => 0,
        _ => wait_for.ordinal(),
    };

    let mut background_cmds: Vec<String> = Vec::new();

    for (i, &phase) in phases.iter().enumerate() {
        let is_foreground = i < wait_index;
        let entries =
            resolve_entries_with_metadata(resolved_features, image_metadata, config, phase);

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
///
/// Handles all three command forms (string, array, object/parallel) and
/// generates proper failure propagation for parallel commands.
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
        // Spawn each command in background, track PIDs, wait and fail if any failed.
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
/// Computes a content hash from the workspace, compares it to the stored hash
/// inside the container at `/tmp/.cella/content_hash`. If different, runs the
/// updateContentCommand and postCreateCommand phases, then writes the new hash.
///
/// # Errors
///
/// Returns an error if any re-run lifecycle command fails.
pub async fn check_and_run_content_update(
    lc_ctx: &LifecycleContext<'_>,
    config: &serde_json::Value,
    metadata: Option<&str>,
    workspace_root: &std::path::Path,
    progress: &ProgressSender,
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
        let entries = lifecycle_entries_for_phase(metadata, config, phase);
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
    use std::path::PathBuf;

    fn make_resolved_features(
        lifecycle: cella_features::FeatureLifecycle,
    ) -> cella_features::ResolvedFeatures {
        cella_features::ResolvedFeatures {
            features: vec![],
            dockerfile: String::new(),
            build_context: PathBuf::from("/tmp"),
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
        // Default (missing key or updateContentCommand) -> UpdateContent
        assert_eq!(
            WaitForPhase::from_config(&json!({})),
            WaitForPhase::UpdateContent
        );
        assert_eq!(
            WaitForPhase::from_config(&json!({"waitFor": "updateContentCommand"})),
            WaitForPhase::UpdateContent
        );
    }

    // ── lifecycle_entries_for_phase: array commands ──────────────────────

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
        // The whole object is preserved as the command value
        assert!(entries[0].command.is_object());
    }

    // ── lifecycle_entries_for_phase: metadata path ───────────────────────

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

    // ── resolve_phase_entries: additional edge cases ─────────────────────

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

    // ── WaitForPhase: ordinal values ─────────────────────────────────────

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

    // ── LifecycleState serde ────────────────────────────────────────────

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

    // ── hash_lifecycle_entries ───────────────────────────────────────────

    #[test]
    fn hash_lifecycle_entries_deterministic() {
        let entries = vec![cella_features::LifecycleEntry {
            origin: "feature-a".into(),
            command: json!("npm install"),
        }];
        let h1 = hash_lifecycle_entries(&entries);
        let h2 = hash_lifecycle_entries(&entries);
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64); // SHA-256 hex
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

    // ── resolve_entries_with_metadata ────────────────────────────────────

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

    // ── entry_to_shell_command ─────────────────────────────────────────

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
        // Must track PIDs and check exit codes
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
