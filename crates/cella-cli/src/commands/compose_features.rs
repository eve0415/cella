//! Compose + features orchestration — delegates to `cella_orchestrator::compose_features`.

pub use cella_orchestrator::compose_features::ComposeFeaturesBuild;

use std::path::Path;

use cella_backend::ContainerBackend;
use cella_compose::ComposeProject;

use crate::progress::Progress;

/// Resolve features for a compose service and generate the combined Dockerfile.
///
/// Thin wrapper bridging the CLI `Progress` to the orchestrator's `ProgressSender`.
pub async fn resolve_compose_features(
    client: &dyn ContainerBackend,
    config: &serde_json::Value,
    config_path: &Path,
    project: &ComposeProject,
    _no_cache: bool,
    progress: &Progress,
) -> Result<Option<ComposeFeaturesBuild>, Box<dyn std::error::Error>> {
    let (sender, renderer) = crate::progress::bridge(progress);
    let result = cella_orchestrator::compose_features::resolve_compose_features(
        client,
        config,
        config_path,
        project,
        &sender,
    )
    .await
    .map_err(|e| e.to_string());
    drop(sender);
    let _ = renderer.await;
    result.map_err(|e| -> Box<dyn std::error::Error> { e.into() })
}
