//! Lifecycle phase management — delegates to `cella_orchestrator::lifecycle`.
//!
//! These wrappers bridge the CLI's `Progress` type to the orchestrator's
//! `ProgressSender` for functions called directly from `up/mod.rs`.

use cella_backend::LifecycleContext;

use crate::progress::Progress;

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
