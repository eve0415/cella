//! Image resolution — delegates to `cella_orchestrator::image`.
//!
//! This module bridges the CLI's `Progress` type to the orchestrator's
//! `ProgressSender`. Callers pass `&Progress`; internally we create a
//! channel, spawn a renderer, call the orchestrator, then clean up.

use std::path::Path;

use cella_backend::{ContainerBackend, ImageDetails};
use cella_features::ResolvedFeatures;

use crate::progress::Progress;

// Re-export items that are used directly by other CLI modules.
pub use cella_orchestrator::image::compute_features_digest;

/// Ensure the dev container image exists (pull or build), including features layer.
///
/// Thin wrapper: creates a `ProgressSender` bridge, calls the orchestrator,
/// and renders progress events via indicatif.
pub async fn ensure_image(
    client: &dyn ContainerBackend,
    config: &serde_json::Value,
    workspace_root: &Path,
    config_name: Option<&str>,
    config_path: &Path,
    no_cache: bool,
    progress: &Progress,
) -> Result<(String, Option<ResolvedFeatures>, ImageDetails), Box<dyn std::error::Error>> {
    let (sender, renderer) = crate::progress::bridge(progress);

    let result = cella_orchestrator::image::ensure_image(
        client,
        config,
        workspace_root,
        config_name,
        config_path,
        no_cache,
        &sender,
    )
    .await;

    // Drop sender first so the renderer sees channel close.
    drop(sender);

    // Consume the non-Send Box<dyn Error> before awaiting the renderer,
    // because holding it across an await triggers clippy::future_not_send.
    let output = result?;
    let _ = renderer.await;
    Ok(output)
}
