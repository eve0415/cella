//! Docker Compose build pipeline.
//!
//! Resolves features, writes the compose override file, and runs
//! `docker compose build`. Shared by both `cella build` and `cella up`
//! (compose path).

use std::path::{Path, PathBuf};

use cella_backend::{BuildSecret, ContainerBackend};
use cella_compose::ComposeProject;

use crate::progress::ProgressSender;

/// Result of a compose build operation.
pub struct ComposeBuildResult {
    /// The primary image name (or `"(compose)"` if no features were resolved).
    pub image_name: String,
    /// Whether features were resolved and a combined Dockerfile was generated.
    pub had_features: bool,
}

/// Configuration for a compose build invocation.
pub struct ComposeBuildConfig<'a> {
    /// Parsed devcontainer JSON.
    pub config: &'a serde_json::Value,
    /// Path to devcontainer.json.
    pub config_path: &'a Path,
    /// Workspace root on the host.
    pub workspace_root: &'a Path,
    /// Docker Compose profiles to activate (`--profile` flags).
    pub profiles: Vec<String>,
    /// Extra env-file paths for docker compose (`--env-file` flags).
    pub env_files: Vec<PathBuf>,
    /// Pull policy for docker compose build (`--pull` flag).
    pub pull_policy: Option<String>,
    /// `BuildKit` secrets forwarded to compose builds.
    pub secrets: Vec<BuildSecret>,
}

/// Run the compose build pipeline: resolve features, write override, build.
///
/// # Errors
///
/// Returns an error if feature resolution, override generation, or the
/// compose build command fails.
pub async fn compose_build(
    client: &dyn ContainerBackend,
    cfg: &ComposeBuildConfig<'_>,
    progress: &ProgressSender,
) -> Result<ComposeBuildResult, Box<dyn std::error::Error + Send + Sync>> {
    if !client.capabilities().compose {
        return Err(format!(
            "selected backend '{}' does not support Docker Compose devcontainers",
            client.kind()
        )
        .into());
    }

    let config = cfg.config;
    let config_path = cfg.config_path;
    let workspace_root = cfg.workspace_root;
    let mut project = ComposeProject::from_resolved(config, config_path, workspace_root)?;
    project.set_compose_options(
        cfg.profiles.clone(),
        cfg.env_files.clone(),
        cfg.pull_policy.clone(),
    );

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
        let compose_secrets: Vec<cella_compose::ComposeSecret> = cfg
            .secrets
            .iter()
            .map(|s| cella_compose::ComposeSecret {
                id: s.id.clone(),
                file: s.src.clone(),
                environment: s.env.clone(),
            })
            .collect();
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
            build_secrets: compose_secrets,
            extra_volumes: Vec::new(),
        };
        let yaml = cella_compose::override_file::generate_override_yaml(&override_config);
        cella_compose::override_file::write_override_file(&project.override_file, &yaml)?;
    }

    // Run docker compose build
    let compose_cmd = cella_compose::ComposeCommand::new(&project);
    compose_cmd.build(None, false).await.map_err(
        |e| -> Box<dyn std::error::Error + Send + Sync> {
            format!("docker compose build failed: {e}").into()
        },
    )?;

    let had_features = features_build.is_some();
    let image_name = features_build
        .and_then(|b| b.image_name_override)
        .unwrap_or_else(|| "(compose)".to_string());

    Ok(ComposeBuildResult {
        image_name,
        had_features,
    })
}
