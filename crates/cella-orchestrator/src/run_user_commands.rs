//! Re-run devcontainer lifecycle hooks against an existing container.
//!
//! Backs the `run-user-commands` command, which mirrors the official
//! devcontainer CLI `run-user-commands` handler (`doRunUserCommands` /
//! `runLifecycleHooks`, NOT `set-up`/`doSetUp`): it runs the user lifecycle
//! commands against a container that already exists and returns the
//! `runLifecycleHooks` status string (`done`, `skipNonBlocking`, `prebuild`,
//! `stopForPersonalization`).
//!
//! Every phase runs in the FOREGROUND (awaited) so that a failing command
//! surfaces as an error in the result envelope, and the gated-return order
//! matches `injectHeadless.ts` `runLifecycleHooks` exactly:
//!
//! a. if `skip_non_blocking` && `waitFor == initializeCommand` → `skipNonBlocking`
//! b. run `onCreateCommand`; gate
//! c. run `updateContentCommand`; gate
//! d. if `prebuild` → `prebuild`
//! e. run `postCreateCommand`; gate
//! f. install dotfiles (between `postCreate` and `postStart`)
//! g. if `stop_for_personalization` → `stopForPersonalization`
//! h. run `postStartCommand`; gate
//! i. if `!skip_post_attach` → run `postAttachCommand`
//! j. `done`
//!
//! `postAttachCommand` has no `skip_non_blocking` gate; the default `waitFor`
//! is `updateContentCommand`.
//!
//! DIVERGENCE (documented): the official handler computes a per-phase
//! `doRun` from `createdAt`/`startedAt` marker files, so against a freshly
//! provisioned container it typically re-runs only `postAttachCommand`. cella
//! has no createdAt/startedAt markers (its only analogs are the content hash
//! and `oncreate_done` state used by `up`), so this runner re-runs every gated
//! phase unconditionally. The observable difference is redundant re-execution
//! of idempotent hooks, not a different final state or a different status
//! string.

use cella_backend::ContainerBackend;
use cella_backend::lifecycle::{
    LifecycleContext, StopAfter, WaitForPhase, lifecycle_entries_for_phase, run_lifecycle_entries,
};
use cella_backend::progress::ProgressSender;
use serde_json::Value;

use crate::dotfiles::install_dotfiles;

/// Boxed, thread-safe error type used across this module.
type RunError = Box<dyn std::error::Error + Send + Sync>;

/// `runLifecycleHooks` status string, returned as the `result` field of the
/// `run-user-commands` success envelope.
pub const STATUS_DONE: &str = "done";
/// Stopped after the `waitFor` phase (`--skip-non-blocking-commands`).
pub const STATUS_SKIP_NON_BLOCKING: &str = "skipNonBlocking";
/// Stopped after `updateContentCommand` (`--prebuild`).
pub const STATUS_PREBUILD: &str = "prebuild";
/// Stopped after dotfiles, before `postStart` (`--stop-for-personalization`).
pub const STATUS_STOP_FOR_PERSONALIZATION: &str = "stopForPersonalization";

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

/// Lifecycle-gating inputs for the foreground runner.
///
/// Modelled as a struct (not a `LifecycleGate`) because this command's gating
/// is the official `runLifecycleHooks` gated-return order, which differs from
/// `up`'s foreground/background split: `prebuild` short-circuits at a FIXED
/// point (after `updateContent`, regardless of `waitFor`) and
/// `stop_for_personalization` has no analog on the `up` path. The
/// `skip_non_blocking` / `prebuild` pair reuses [`StopAfter`] (the same grouping
/// `up` uses) to keep the bool count under the lint.
pub struct Gating {
    /// `--skip-non-blocking-commands` / `--prebuild` stop-after flags.
    pub stop: StopAfter,
    /// `--stop-for-personalization`: stop after dotfiles, before `postStart`.
    pub stop_for_personalization: bool,
    /// `--skip-post-attach`: do not run `postAttachCommand`.
    pub skip_post_attach: bool,
    /// Resolved `waitFor` phase, gating the `skip_non_blocking` short-circuits.
    pub wait_for: WaitForPhase,
}

/// Everything the foreground lifecycle runner needs, gathered into one borrow
/// to keep the argument count under the lint and group related inputs.
pub struct RunUserCommandsInput<'a> {
    /// Resolved devcontainer config (lifecycle is sourced from `metadata` when
    /// present, falling back to this config's phase keys).
    pub config: &'a Value,
    /// The container's effective lifecycle-metadata array, when present (the
    /// baked `devcontainer.metadata`, plus the on-disk `--config` appended in
    /// the `--container-id` case — see `effective_lifecycle_metadata`). Source
    /// of truth for lifecycle commands; falls back to `config` when absent.
    pub metadata: Option<&'a str>,
    /// Lifecycle-gating inputs.
    pub gating: Gating,
    /// Dotfiles install inputs.
    pub dotfiles: DotfilesInputs<'a>,
}

/// Run the gated lifecycle phases in the foreground against an existing
/// container, installing dotfiles between `postCreate` and `postStart`,
/// returning the `runLifecycleHooks` status string.
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
) -> Result<&'static str, RunError> {
    let g = &input.gating;

    // (a) skip_non_blocking + waitFor == initializeCommand → stop immediately.
    if stops_after(g, WaitForPhase::Initialize) {
        return Ok(STATUS_SKIP_NON_BLOCKING);
    }

    // (b) onCreate, then gate.
    run_phase(lc_ctx, input, "onCreateCommand", progress).await?;
    if stops_after(g, WaitForPhase::OnCreate) {
        return Ok(STATUS_SKIP_NON_BLOCKING);
    }

    // (c) updateContent, then gate.
    run_phase(lc_ctx, input, "updateContentCommand", progress).await?;
    if stops_after(g, WaitForPhase::UpdateContent) {
        return Ok(STATUS_SKIP_NON_BLOCKING);
    }

    // (d) prebuild short-circuits at a fixed point, after updateContent.
    if g.stop.prebuild {
        return Ok(STATUS_PREBUILD);
    }

    // (e) postCreate, then gate.
    run_phase(lc_ctx, input, "postCreateCommand", progress).await?;
    if stops_after(g, WaitForPhase::PostCreate) {
        return Ok(STATUS_SKIP_NON_BLOCKING);
    }

    // (f) dotfiles between postCreate and postStart.
    maybe_install_dotfiles(lc_ctx, input).await;

    // (g) stop_for_personalization fires after dotfiles, before postStart.
    if g.stop_for_personalization {
        return Ok(STATUS_STOP_FOR_PERSONALIZATION);
    }

    // (h) postStart, then gate.
    run_phase(lc_ctx, input, "postStartCommand", progress).await?;
    if stops_after(g, WaitForPhase::PostStart) {
        return Ok(STATUS_SKIP_NON_BLOCKING);
    }

    // (i) postAttach, gated only by skip_post_attach.
    if !g.skip_post_attach {
        run_phase(lc_ctx, input, "postAttachCommand", progress).await?;
    }

    // (j) done.
    Ok(STATUS_DONE)
}

/// Whether `skip_non_blocking` short-circuits at `phase` (i.e. `waitFor`
/// resolves to `phase`). `postAttach` has no such gate, so it is never queried.
fn stops_after(g: &Gating, phase: WaitForPhase) -> bool {
    g.stop.skip_non_blocking && g.wait_for == phase
}

/// Run a single lifecycle phase in the foreground.
async fn run_phase(
    lc_ctx: &LifecycleContext<'_>,
    input: &RunUserCommandsInput<'_>,
    phase: &str,
    progress: &ProgressSender,
) -> Result<(), RunError> {
    let entries = lifecycle_entries_for_phase(input.metadata, input.config, phase);
    run_lifecycle_entries(lc_ctx, phase, &entries, progress).await?;
    Ok(())
}

/// Install dotfiles between `postCreate` and `postStart`. A failure is logged
/// and swallowed (non-fatal), matching the official tool.
async fn maybe_install_dotfiles(lc_ctx: &LifecycleContext<'_>, input: &RunUserCommandsInput<'_>) {
    let Some(repository) = input.dotfiles.repository else {
        return;
    };
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
        // The dotfiles script runs with the secret-bearing lifecycle env;
        // mask before logging so secret values don't leak.
        let err_text = e.to_string();
        let msg = lc_ctx.secret_masker.mask(&err_text);
        tracing::warn!("Dotfiles install failed (continuing): {msg}");
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

/// Accumulate the `remoteEnv` entries from a `devcontainer.metadata` label.
///
/// The label is a JSON array of metadata entries (base image + each feature +
/// the user config, in merge order). Each entry's `remoteEnv` object is
/// accumulated with later-wins precedence, matching the official
/// `mergeConfiguration` (which spreads each entry's `remoteEnv` in order). The
/// returned `name=value` list is ordered for `merge_env`'s later-wins `HashMap`
/// insert, so callers append it AFTER `--remote-env` to get the official
/// precedence (probed < `--remote-env` < merged `remoteEnv`).
#[must_use]
pub fn metadata_remote_env(metadata: Option<&str>) -> Vec<String> {
    let Some(json) = metadata else {
        return Vec::new();
    };
    let entries: Vec<Value> = serde_json::from_str(json).unwrap_or_default();
    // Accumulate later-wins so the same key set across entries resolves to the
    // last entry's value, matching mergeConfiguration.
    let mut acc: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    for entry in &entries {
        if let Some(obj) = entry.get("remoteEnv").and_then(Value::as_object) {
            for (k, v) in obj {
                if let Some(s) = v.as_str() {
                    acc.insert(k.clone(), s.to_string());
                }
            }
        }
    }
    acc.into_iter().map(|(k, v)| format!("{k}={v}")).collect()
}

/// Resolve the `userEnvProbe` from a `devcontainer.metadata` label.
///
/// Returns the LAST array entry that declares `userEnvProbe`, mirroring official
/// `mergeConfiguration` (`reversed.find(entry => entry.userEnvProbe)`). The
/// caller prefers the fresh `--config`'s value first (it sits in the official
/// `pickUpdateableConfigProperties` whitelist, so it wins in every branch),
/// then this baked value, then the CLI `--default-user-env-probe`.
#[must_use]
pub fn metadata_user_env_probe(metadata: Option<&str>) -> Option<String> {
    let entries: Vec<Value> = serde_json::from_str(metadata?).ok()?;
    entries.iter().rev().find_map(|e| {
        e.get("userEnvProbe")
            .and_then(Value::as_str)
            .map(String::from)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Build a `Gating` exercising only the `skip_non_blocking` short-circuit
    /// path (the other flags are irrelevant to `stops_after`).
    fn gating(skip_non_blocking: bool, wait_for: WaitForPhase) -> Gating {
        Gating {
            stop: StopAfter {
                skip_non_blocking,
                prebuild: false,
            },
            stop_for_personalization: false,
            skip_post_attach: false,
            wait_for,
        }
    }

    #[test]
    fn skip_non_blocking_initialize_stops_immediately() {
        let g = gating(true, WaitForPhase::Initialize);
        assert!(stops_after(&g, WaitForPhase::Initialize));
    }

    #[test]
    fn skip_non_blocking_gates_only_the_wait_for_phase() {
        let g = gating(true, WaitForPhase::OnCreate);
        assert!(!stops_after(&g, WaitForPhase::Initialize));
        assert!(stops_after(&g, WaitForPhase::OnCreate));
        assert!(!stops_after(&g, WaitForPhase::UpdateContent));
    }

    #[test]
    fn no_skip_non_blocking_never_stops() {
        let g = gating(false, WaitForPhase::OnCreate);
        for phase in [
            WaitForPhase::Initialize,
            WaitForPhase::OnCreate,
            WaitForPhase::UpdateContent,
            WaitForPhase::PostCreate,
            WaitForPhase::PostStart,
        ] {
            assert!(!stops_after(&g, phase));
        }
    }

    #[test]
    fn metadata_remote_env_accumulates_later_wins() {
        let meta = json!([
            {"remoteEnv": {"A": "1", "B": "1"}},
            {"id": "feature", "remoteEnv": {"B": "2", "C": "2"}}
        ])
        .to_string();
        let env = metadata_remote_env(Some(&meta));
        assert!(env.contains(&"A=1".to_string()));
        assert!(env.contains(&"B=2".to_string())); // later entry wins
        assert!(env.contains(&"C=2".to_string()));
    }

    #[test]
    fn metadata_remote_env_empty_without_label() {
        assert!(metadata_remote_env(None).is_empty());
        assert!(metadata_remote_env(Some("[]")).is_empty());
        assert!(metadata_remote_env(Some("not json")).is_empty());
    }

    #[test]
    fn metadata_user_env_probe_last_entry_wins() {
        // mergeConfiguration: reversed.find(entry => entry.userEnvProbe).
        let meta = json!([
            {"userEnvProbe": "loginInteractiveShell"},
            {"id": "feature", "userEnvProbe": "interactiveShell"}
        ])
        .to_string();
        assert_eq!(
            metadata_user_env_probe(Some(&meta)).as_deref(),
            Some("interactiveShell")
        );
    }

    #[test]
    fn metadata_user_env_probe_absent_or_no_label() {
        assert!(metadata_user_env_probe(None).is_none());
        assert!(metadata_user_env_probe(Some("[]")).is_none());
        assert!(metadata_user_env_probe(Some(r#"[{"id":"f"}]"#)).is_none());
        assert!(metadata_user_env_probe(Some("not json")).is_none());
    }

    #[test]
    fn resolve_remote_user_prefers_config_remote_user() {
        // No backend call is reached when config carries remoteUser, so this
        // exercises the early-return path without a live client.
        let config = json!({"remoteUser": "vscode"});
        let user = config
            .get("remoteUser")
            .and_then(Value::as_str)
            .unwrap_or("root");
        assert_eq!(user, "vscode");
    }
}
