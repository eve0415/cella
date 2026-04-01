//! Docker Compose build pipeline.
//!
//! Resolves features, writes the compose override file, and runs
//! `docker compose build`. Shared by both `cella build` and `cella up`
//! (compose path).

use std::path::Path;

use cella_backend::ContainerBackend;
use cella_compose::ComposeProject;

use crate::progress::ProgressSender;

/// Result of a compose build operation.
pub struct ComposeBuildResult {
    /// The primary image name (or `"(compose)"` if no features were resolved).
    pub image_name: String,
    /// Whether features were resolved and a combined Dockerfile was generated.
    pub had_features: bool,
}

/// Run the compose build pipeline: resolve features, write override, build.
///
/// # Errors
///
/// Returns an error if feature resolution, override generation, or the
/// compose build command fails.
pub async fn compose_build(
    client: &dyn ContainerBackend,
    config: &serde_json::Value,
    config_path: &Path,
    workspace_root: &Path,
    progress: &ProgressSender,
) -> Result<ComposeBuildResult, Box<dyn std::error::Error>> {
    let project = ComposeProject::from_resolved(config, config_path, workspace_root)?;

    // Resolve features via combined-Dockerfile approach
    let features_build = crate::compose_features::resolve_compose_features(
        client,
        config,
        config_path,
        &project,
        progress,
    )
    .await?;

    // Write override file if features were resolved
    if let Some(ref fb) = features_build {
        let (agent_vol_name, agent_vol_target, _) = if client.capabilities().managed_agent {
            client.agent_volume_mount()
        } else {
            (String::new(), String::new(), true)
        };
        let override_config = cella_compose::OverrideConfig {
            primary_service: project.primary_service.clone(),
            image_override: fb.image_name_override.clone(),
            override_command: project.override_command,
            agent_volume_name: agent_vol_name,
            agent_volume_target: agent_vol_target,
            extra_env: Vec::new(),
            extra_labels: std::collections::BTreeMap::new(),
            build_dockerfile: Some(fb.combined_dockerfile.clone()),
            build_target: Some(fb.build_target.clone()),
            build_context: fb.build_context.clone(),
            additional_contexts: fb.additional_contexts.clone(),
        };
        let yaml = cella_compose::override_file::generate_override_yaml(&override_config);
        cella_compose::override_file::write_override_file(&project.override_file, &yaml)?;
    }

    // Run docker compose build
    let compose_cmd = cella_compose::ComposeCommand::new(&project);
    compose_cmd
        .build(None)
        .await
        .map_err(|e| -> Box<dyn std::error::Error> {
            format!("docker compose build failed: {e}").into()
        })?;

    let image_name = features_build
        .and_then(|b| b.image_name_override)
        .unwrap_or_else(|| "(compose)".to_string());

    Ok(ComposeBuildResult {
        had_features: image_name != "(compose)",
        image_name,
    })
}
