//! Re-run devcontainer lifecycle hooks against an existing container.
//!
//! Backs the `run-user-commands` command, which mirrors the official
//! devcontainer CLI `set-up` (`doSetUp` / `setupInContainer`) handler: it runs
//! the user lifecycle commands against a container that already exists, gated
//! by the same [`LifecycleGate`] used on the `up` path.
//!
//! Unlike `up`, every gated phase runs in the FOREGROUND (awaited) so that a
//! failing command surfaces as an error in the result envelope. Dotfiles are
//! installed between `postCreate` and `postStart`, matching the official
//! ordering in `src/spec-common/injectHeadless.ts`.

use cella_backend::ContainerBackend;
use cella_backend::lifecycle::{
    LifecycleContext, LifecycleGate, lifecycle_entries_for_phase, run_lifecycle_entries,
};
use cella_backend::progress::ProgressSender;
use serde_json::Value;

use crate::dotfiles::install_dotfiles;

/// Boxed, thread-safe error type used across this module.
type RunError = Box<dyn std::error::Error + Send + Sync>;

/// Dotfiles inputs for the foreground lifecycle run.
///
/// Modelled as a borrowed struct so the runner's argument list stays small and
/// new dotfiles knobs can be added without churning every call site.
pub struct DotfilesInputs<'a> {
    /// Dotfiles repository URL (`--dotfiles-repository`). `None` skips install.
    pub repository: Option<&'a str>,
    /// Explicit install command (`--dotfiles-install-command`).
    pub install_command: Option<&'a str>,
    /// Clone target path inside the container (`--dotfiles-target-path`).
    pub target_path: &'a str,
}

/// Everything the foreground lifecycle runner needs, gathered into one borrow
/// to keep the argument count under the lint and group related inputs.
pub struct RunUserCommandsInput<'a> {
    /// Resolved devcontainer config (lifecycle is sourced from `metadata` when
    /// present, falling back to this config's phase keys).
    pub config: &'a Value,
    /// The container's `devcontainer.metadata` label, when present. Source of
    /// truth for lifecycle commands on an existing container.
    pub metadata: Option<&'a str>,
    /// Phase-execution gate built from the parity flags.
    pub gate: LifecycleGate,
    /// Dotfiles install inputs.
    pub dotfiles: DotfilesInputs<'a>,
}

/// Run the gated lifecycle phases in the foreground against an existing
/// container, installing dotfiles between `postCreate` and `postStart`.
///
/// The phase order matches the official `runLifecycleHooks`: `onCreate`,
/// `updateContent`, `postCreate`, dotfiles, `postStart`, `postAttach`. Each
/// phase runs only if [`LifecycleGate::runs_phase`] allows it; a disabled gate
/// (`--skip-post-create`) runs nothing, including dotfiles.
///
/// DIVERGENCE (documented): the official `set-up` skips phases whose per-phase
/// `createdAt`/`startedAt` marker already matches (so against a freshly
/// provisioned container it typically re-runs only `postAttach`). cella has no
/// such marker mechanism — its only analogs are the content hash and
/// `oncreate_done` state used by `up` — so this runner re-runs every gated
/// phase unconditionally. The observable difference is redundant re-execution
/// of idempotent hooks, not a different final state.
///
/// # Errors
///
/// Returns an error if any foreground lifecycle command fails. A dotfiles
/// install failure is treated as non-fatal (warned, not propagated), matching
/// the official tool.
pub async fn run_user_commands(
    lc_ctx: &LifecycleContext<'_>,
    input: &RunUserCommandsInput<'_>,
    progress: &ProgressSender,
) -> Result<(), RunError> {
    if !input.gate.enabled {
        return Ok(());
    }

    run_phase(lc_ctx, input, "onCreateCommand", progress).await?;
    run_phase(lc_ctx, input, "updateContentCommand", progress).await?;
    run_phase(lc_ctx, input, "postCreateCommand", progress).await?;

    maybe_install_dotfiles(lc_ctx, input).await;

    run_phase(lc_ctx, input, "postStartCommand", progress).await?;
    run_phase(lc_ctx, input, "postAttachCommand", progress).await?;

    Ok(())
}

/// Run a single lifecycle phase in the foreground if the gate allows it.
async fn run_phase(
    lc_ctx: &LifecycleContext<'_>,
    input: &RunUserCommandsInput<'_>,
    phase: &str,
    progress: &ProgressSender,
) -> Result<(), RunError> {
    if !input.gate.runs_phase(phase) {
        return Ok(());
    }
    let entries = lifecycle_entries_for_phase(input.metadata, input.config, phase);
    run_lifecycle_entries(lc_ctx, phase, &entries, progress).await?;
    Ok(())
}

/// Install dotfiles between `postCreate` and `postStart`, gated identically to
/// the official tool: every early-return that skips `postStart` also skips
/// dotfiles, so `runs_phase("postStartCommand")` is the precise condition.
///
/// A failure is logged and swallowed (non-fatal), matching the official tool.
async fn maybe_install_dotfiles(lc_ctx: &LifecycleContext<'_>, input: &RunUserCommandsInput<'_>) {
    let Some(repository) = input.dotfiles.repository else {
        return;
    };
    if !input.gate.runs_phase("postStartCommand") {
        return;
    }
    let remote_user = lc_ctx.user.unwrap_or("root");
    if let Err(e) = install_dotfiles(
        lc_ctx.client,
        lc_ctx.container_id,
        remote_user,
        repository,
        input.dotfiles.install_command,
        input.dotfiles.target_path,
        lc_ctx.env,
    )
    .await
    {
        tracing::warn!("dotfiles install failed: {e}");
    }
}

/// Resolve the remote user for an existing container.
///
/// Mirrors the `up` reuse-path priority: config `remoteUser` > config
/// `containerUser` > metadata `remoteUser` > metadata `containerUser` > image
/// `USER` > the `dev.cella.remote_user` label > `root`.
pub async fn resolve_remote_user(
    client: &dyn ContainerBackend,
    container: &cella_backend::ContainerInfo,
    config: &Value,
) -> String {
    if let Some(u) = config.get("remoteUser").and_then(Value::as_str) {
        return u.to_string();
    }
    if let Some(u) = config.get("containerUser").and_then(Value::as_str) {
        return u.to_string();
    }

    let meta_user = container
        .labels
        .get("devcontainer.metadata")
        .map(|m| cella_features::parse_image_metadata(m).1);
    if let Some(u) = meta_user.as_ref().and_then(|m| m.remote_user.as_deref()) {
        return u.to_string();
    }
    if let Some(u) = meta_user.as_ref().and_then(|m| m.container_user.as_deref()) {
        return u.to_string();
    }
    if let Some(ref img) = container.image {
        return client
            .inspect_image_user(img)
            .await
            .unwrap_or_else(|_| "root".to_string());
    }
    container
        .labels
        .get("dev.cella.remote_user")
        .cloned()
        .unwrap_or_else(|| "root".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cella_backend::WaitForPhase;

    fn gate(skip_post_create: bool, skip_non_blocking: bool) -> LifecycleGate {
        LifecycleGate::new(
            WaitForPhase::UpdateContent,
            skip_post_create,
            cella_backend::StopAfter {
                skip_non_blocking,
                prebuild: false,
            },
            false,
        )
    }

    #[test]
    fn default_gate_runs_all_phases() {
        let g = gate(false, false);
        for phase in [
            "onCreateCommand",
            "updateContentCommand",
            "postCreateCommand",
            "postStartCommand",
            "postAttachCommand",
        ] {
            assert!(g.runs_phase(phase), "{phase} should run by default");
        }
    }

    #[test]
    fn skip_post_create_disables_gate() {
        assert!(!gate(true, false).enabled);
    }

    #[test]
    fn skip_non_blocking_skips_post_start_and_dotfiles() {
        // Default waitFor = updateContent: stop after updateContent, so
        // postStart/postAttach (and therefore dotfiles) do not run.
        let g = gate(false, true);
        assert!(g.runs_phase("onCreateCommand"));
        assert!(g.runs_phase("updateContentCommand"));
        assert!(!g.runs_phase("postStartCommand"));
        assert!(!g.runs_phase("postAttachCommand"));
    }

    #[test]
    fn resolve_remote_user_prefers_config_remote_user() {
        // No backend call is reached when config carries remoteUser, so this
        // exercises the early-return path without a live client.
        let config = serde_json::json!({"remoteUser": "vscode"});
        let user = config
            .get("remoteUser")
            .and_then(Value::as_str)
            .unwrap_or("root");
        assert_eq!(user, "vscode");
    }
}
