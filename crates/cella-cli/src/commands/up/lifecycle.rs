//! Lifecycle phase management — delegates to `cella_orchestrator::lifecycle`.
//!
//! These wrappers bridge the CLI's `Progress` type to the orchestrator's
//! `ProgressSender` for functions called directly from `up/mod.rs`.

use cella_docker::{CellaDockerError, DockerClient, LifecycleContext};

use crate::progress::Progress;

// Re-export types that don't need bridging.
pub use cella_orchestrator::lifecycle::WaitForPhase;

/// Run all lifecycle phases for a first-create scenario.
pub async fn run_all_lifecycle_phases(
    lc_ctx: &LifecycleContext<'_>,
    config: &serde_json::Value,
    resolved_features: Option<&cella_features::ResolvedFeatures>,
    progress: &Progress,
) -> Result<(), Box<dyn std::error::Error>> {
    let (sender, renderer) = crate::progress::bridge(progress);
    let result = cella_orchestrator::lifecycle::run_all_lifecycle_phases(
        lc_ctx,
        config,
        resolved_features,
        &sender,
    )
    .await;
    drop(sender);
    result?;
    let _ = renderer.await;
    Ok(())
}

/// Run lifecycle phases up to `wait_for`, then spawn remaining in background.
pub async fn run_lifecycle_phases_with_wait_for(
    lc_ctx: &LifecycleContext<'_>,
    config: &serde_json::Value,
    resolved_features: Option<&cella_features::ResolvedFeatures>,
    progress: &Progress,
    wait_for: WaitForPhase,
) -> Result<(), Box<dyn std::error::Error>> {
    let (sender, renderer) = crate::progress::bridge(progress);
    let result = cella_orchestrator::lifecycle::run_lifecycle_phases_with_wait_for(
        lc_ctx,
        config,
        resolved_features,
        &sender,
        wait_for,
    )
    .await;
    drop(sender);
    result?;
    let _ = renderer.await;
    Ok(())
}

/// Run a sequence of lifecycle entries with progress tracking.
pub(super) async fn run_lifecycle_entries(
    ctx: &LifecycleContext<'_>,
    phase: &str,
    entries: &[cella_features::LifecycleEntry],
    progress: &Progress,
) -> Result<(), CellaDockerError> {
    let (sender, renderer) = crate::progress::bridge(progress);
    let result =
        cella_orchestrator::lifecycle::run_lifecycle_entries(ctx, phase, entries, &sender).await;
    drop(sender);
    result?;
    let _ = renderer.await;
    Ok(())
}

/// Check for workspace content changes and re-run lifecycle phases.
pub(super) async fn check_and_run_content_update(
    lc_ctx: &LifecycleContext<'_>,
    config: &serde_json::Value,
    metadata: Option<&str>,
    workspace_root: &std::path::Path,
    progress: &Progress,
) -> Result<(), Box<dyn std::error::Error>> {
    let (sender, renderer) = crate::progress::bridge(progress);
    let result = cella_orchestrator::lifecycle::check_and_run_content_update(
        lc_ctx,
        config,
        metadata,
        workspace_root,
        &sender,
    )
    .await;
    drop(sender);
    result?;
    let _ = renderer.await;
    Ok(())
}

/// Store the workspace content hash inside the container.
pub(super) async fn write_content_hash(
    client: &DockerClient,
    container_id: &str,
    user: &str,
    workspace_root: &std::path::Path,
) {
    cella_orchestrator::lifecycle::write_content_hash(client, container_id, user, workspace_root)
        .await;
}
