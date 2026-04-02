//! Lifecycle phase management: resolution, execution, and content tracking.
//!
//! Moved from `cella-cli/src/commands/up/lifecycle.rs`. Uses
//! [`ProgressSender`] instead of the indicatif-coupled `Progress` type.

use tracing::debug;

use cella_backend::{
    BackendError, ContainerBackend, ExecOptions, LifecycleContext, run_lifecycle_phase,
};

use crate::progress::{ProgressSender, format_elapsed};

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

/// Run all lifecycle phases for a first-create scenario.
///
/// # Errors
///
/// Returns an error if any lifecycle command fails.
pub async fn run_all_lifecycle_phases(
    lc_ctx: &LifecycleContext<'_>,
    config: &serde_json::Value,
    resolved_features: Option<&cella_features::ResolvedFeatures>,
    progress: &ProgressSender,
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
///
/// # Errors
///
/// Returns an error if any foreground lifecycle command fails.
pub async fn run_lifecycle_phases_with_wait_for(
    lc_ctx: &LifecycleContext<'_>,
    config: &serde_json::Value,
    resolved_features: Option<&cella_features::ResolvedFeatures>,
    progress: &ProgressSender,
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
    let wait_index = match wait_for {
        WaitForPhase::Initialize => 0,
        _ => wait_for.ordinal(),
    };

    let mut background_cmds: Vec<String> = Vec::new();

    for (i, &phase) in phases.iter().enumerate() {
        let is_foreground = i < wait_index;
        let entries = resolve_phase_entries(resolved_features, phase);

        if is_foreground {
            run_lifecycle_entries(lc_ctx, phase, entries, progress).await?;

            if entries.is_empty()
                && let Some(cmd) = config.get(phase)
                && !cmd.is_null()
            {
                run_config_phase_with_output(lc_ctx, phase, cmd, progress).await?;
            }
        } else {
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
) -> Result<(), Box<dyn std::error::Error>> {
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
}
