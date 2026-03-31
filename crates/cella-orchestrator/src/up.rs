//! Core "ensure container is up" pipeline.
//!
//! This module defines the types and trait needed for the full container-up
//! lifecycle. The actual implementation will be extracted from the CLI in a
//! follow-up commit.

use std::future::Future;
use std::pin::Pin;

use crate::error::OrchestratorError;
use crate::progress::ProgressSender;
use crate::result::UpResult;

/// Whether network blocking rules are enforced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetworkRulePolicy {
    /// Enforce blocking rules from config.
    Enforce,
    /// Skip blocking rules (e.g. `--no-network-rules`).
    Skip,
}

/// Callbacks for host-specific actions during container up.
///
/// The CLI provides implementations that register with the daemon;
/// the daemon provides implementations that act internally.
pub trait UpHooks: Send + Sync {
    /// Called after the container is created and started.
    fn on_container_started(
        &self,
        container_id: &str,
        container_name: &str,
        container_ip: Option<&str>,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
}

/// No-op hooks for contexts that don't need daemon registration.
pub struct NoOpHooks;

impl UpHooks for NoOpHooks {
    fn on_container_started(
        &self,
        _container_id: &str,
        _container_name: &str,
        _container_ip: Option<&str>,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async {})
    }
}

/// Run the full container-up pipeline: resolve config, build/pull image,
/// create or restart container, execute lifecycle hooks.
///
/// # Errors
///
/// Returns an error if any stage of the pipeline fails.
pub async fn ensure_up(
    _config: &crate::config::UpConfig,
    _network_rules: NetworkRulePolicy,
    _hooks: &dyn UpHooks,
    _progress: ProgressSender,
) -> Result<UpResult, OrchestratorError> {
    // Yield once so the function is genuinely async. The real implementation
    // will replace this entire body.
    tokio::task::yield_now().await;
    todo!("Will be extracted from CLI in a follow-up commit")
}
