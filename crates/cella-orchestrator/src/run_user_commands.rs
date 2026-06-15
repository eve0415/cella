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
//! PHASE SKIPPING: the official handler gates onCreate/updateContent/postCreate
//! on per-phase `createdAt` marker files (run once per container) and postStart
//! on `startedAt`. cella has no createdAt/startedAt markers; its analogs are the
//! `oncreate_done` flag and content hash in `/tmp/.cella/` that `up` writes. This
//! runner mirrors `up`'s per-phase model fully: it reads those markers up front
//! to skip `onCreateCommand` when `oncreate_done` and `updateContentCommand`/
//! `postCreateCommand` when the workspace content hash is unchanged (with
//! `--prebuild` force-running updateContent, matching the official
//! `rerun = !!prebuild`), AND it writes each marker back after its phase
//! succeeds (just like `up`), so repeated invocations don't re-run a phase that
//! already completed. A FAILED previous background lifecycle re-runs every gated
//! phase (recovery). `postStartCommand` is skipped when the container has not
//! restarted since `up` last ran it (its current `started_at` matches the
//! recorded one), and the new `started_at` is written back after it runs —
//! mirroring the official `startedAt` marker. `postAttachCommand` always runs.

use cella_backend::ContainerBackend;
use cella_backend::lifecycle::{
    LifecycleContext, StopAfter, WaitForPhase, content_changed, lifecycle_entries_for_phase,
    lifecycle_failed, read_lifecycle_state, run_lifecycle_entries, write_content_hash,
    write_lifecycle_state,
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

/// Per-phase skip decisions computed before the phase sequence runs.
///
/// Computed once by [`phase_skips`] and applied to individual `run_phase`
/// calls so that the skip logic is unit-testable independently of I/O.
struct PhaseSkips {
    /// `false` → skip `onCreateCommand` (oncreate already ran).
    run_oncreate: bool,
    /// `false` → skip `updateContentCommand` (content unchanged and not prebuild).
    run_update_content: bool,
    /// `false` → skip `postCreateCommand` (content unchanged).
    run_post_create: bool,
}

impl PhaseSkips {
    /// Run every gated phase — the recovery path, taken when the previous
    /// background lifecycle recorded a FAILURE. The content-hash skip is only
    /// SOUND when the prior run completed: a failed background `postCreateCommand`
    /// leaves the content hash matching, so without this `run-user-commands`
    /// (cella's recovery path) would skip the very phase the user is re-running.
    /// `lifecycle_status.json` is a single coarse signal, so any failure re-runs
    /// all gated phases — matching pre-skip `run-user-commands` behavior.
    const fn run_all() -> Self {
        Self {
            run_oncreate: true,
            run_update_content: true,
            run_post_create: true,
        }
    }
}

/// Compute which lifecycle phases to skip based on persisted container state,
/// mirroring `up`'s per-phase gating (NOT the official's lumped `createdAt`):
///
/// - `oncreate_done=true` → `onCreateCommand` already ran; skip it.
/// - `content_changed=false` → workspace content is unchanged; skip
///   `updateContentCommand` and `postCreateCommand`.
/// - `prebuild` force-runs `updateContentCommand` even when content is
///   unchanged, matching the official `rerun = !!params.prebuild`
///   (injectHeadless.ts) and cella's own `up` reuse path
///   (`gate.rerun_update_content`). `postCreateCommand` is unreachable under
///   prebuild (the flow returns `STATUS_PREBUILD` first), so it stays gated on
///   content alone.
///
/// The recovery path (previous lifecycle failed) is handled by the caller via
/// [`PhaseSkips::run_all`], not here.
const fn phase_skips(oncreate_done: bool, content_changed: bool, prebuild: bool) -> PhaseSkips {
    PhaseSkips {
        run_oncreate: !oncreate_done,
        run_update_content: content_changed || prebuild,
        // `|| prebuild` is intentionally omitted: `run_user_commands` returns
        // STATUS_PREBUILD after updateContent (before postCreate is reached), so
        // postCreate is unreachable under prebuild and needs no override.
        run_post_create: content_changed,
    }
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
    /// Host-side workspace root used for content-hash comparison.
    ///
    /// When `None` (e.g. bare `--container-id` with no resolvable workspace),
    /// `content_changed` defaults to `true` so updateContent/postCreate run
    /// unconditionally — the safe fallback that matches today's behavior.
    pub workspace_root: Option<&'a std::path::Path>,
    /// The container's current `started_at` timestamp (RFC3339, from Docker
    /// inspect). Used to decide whether `postStartCommand` can be skipped.
    ///
    /// `None` means the value was not available (list-path resolution, Apple
    /// backend, etc.) — `postStartCommand` always runs in that case (safe).
    pub container_started_at: Option<&'a str>,
}

/// Decide whether `postStartCommand` should run.
///
/// Skip only when the recorded `started_at` (what `up` wrote after the last
/// successful postStart) matches the container's current `started_at` — i.e.
/// the container has NOT restarted since postStart last ran.
///
/// `None` on either side → run (safe: no recorded value means we haven't run
/// yet, or the current value is unavailable from the backend).
#[must_use]
pub fn should_run_post_start(recorded: Option<&str>, current: Option<&str>) -> bool {
    // Skip only on a confirmed match of two `Some` values.
    !(recorded.is_some() && recorded == current)
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
    // Read phase-skip state AFTER this early return so the round-trips are
    // skipped on the fast path.
    if stops_after(g, WaitForPhase::Initialize) {
        return Ok(STATUS_SKIP_NON_BLOCKING);
    }

    // Read oncreate_done and content-changed state once, before the phase
    // sequence, so every skip decision uses a consistent snapshot.
    let remote_user = lc_ctx.user.unwrap_or("root");
    // Read the full state (not just the flag) so the marker write-back below is a
    // read-modify-write that preserves any fields a later PR adds.
    let mut lc_state = read_lifecycle_state(lc_ctx.client, lc_ctx.container_id, remote_user).await;
    let oncreate_done = lc_state.oncreate_done;
    // Under prebuild, content_changed only feeds run_post_create, which is
    // unreachable (the flow returns STATUS_PREBUILD before postCreate), and
    // run_update_content is forced anyway — so skip the workspace scan. No
    // workspace_root (bare --container-id) → default to changed (safe).
    let is_content_changed = if g.stop.prebuild {
        false
    } else {
        match input.workspace_root {
            Some(ws) => content_changed(lc_ctx, ws).await,
            None => true,
        }
    };
    // If the previous background lifecycle FAILED, re-run every gated phase —
    // run-user-commands is cella's recovery path, and the content-hash skip is
    // only sound when the prior run completed.
    let recovery = lifecycle_failed(lc_ctx).await;
    let skips = if recovery {
        PhaseSkips::run_all()
    } else {
        phase_skips(oncreate_done, is_content_changed, g.stop.prebuild)
    };
    // postStart is gated separately (restart detection, not create-time state):
    // skip it only when the container hasn't restarted since `up` last ran it.
    // Recovery forces it like every other phase.
    let run_post_start = recovery
        || should_run_post_start(lc_state.started_at.as_deref(), input.container_started_at);

    // (b) onCreate, then gate. Mark it done on success so a later run/up skips
    // it — written only after the phase ran, mirroring `up`.
    if skips.run_oncreate {
        run_phase(lc_ctx, input, "onCreateCommand", progress).await?;
        lc_state.oncreate_done = true;
        write_lifecycle_state(lc_ctx.client, lc_ctx.container_id, remote_user, &lc_state).await;
    }
    if stops_after(g, WaitForPhase::OnCreate) {
        return Ok(STATUS_SKIP_NON_BLOCKING);
    }

    // (c) updateContent, then gate.
    if skips.run_update_content {
        run_phase(lc_ctx, input, "updateContentCommand", progress).await?;
    }
    if stops_after(g, WaitForPhase::UpdateContent) {
        return Ok(STATUS_SKIP_NON_BLOCKING);
    }

    // (d) prebuild short-circuits at a fixed point, after updateContent.
    if g.stop.prebuild {
        return Ok(STATUS_PREBUILD);
    }

    // (e) postCreate, then gate. Persist the content hash on success (only when
    // there's a workspace to hash) so a later run/up skips the content phases for
    // unchanged content — mirrors `up`'s `if run_post_create { write_content_hash }`.
    if skips.run_post_create {
        run_phase(lc_ctx, input, "postCreateCommand", progress).await?;
        if let Some(ws) = input.workspace_root {
            write_content_hash(lc_ctx.client, lc_ctx.container_id, remote_user, ws).await;
        }
    }
    if stops_after(g, WaitForPhase::PostCreate) {
        return Ok(STATUS_SKIP_NON_BLOCKING);
    }

    // (f) dotfiles between postCreate and postStart.
    maybe_install_dotfiles(lc_ctx, input).await;

    // (g) stop_for_personalization fires after dotfiles, before postStart.
    if g.stop_for_personalization {
        return Ok(STATUS_STOP_FOR_PERSONALIZATION);
    }

    // (h) postStart, then gate. Skip when the container hasn't restarted since
    // `up` last ran it; on success record the current started_at so a later
    // run/up skips it — written only after the phase ran, mirroring `up`.
    if run_post_start {
        run_phase(lc_ctx, input, "postStartCommand", progress).await?;
        if let Some(current) = input.container_started_at {
            lc_state.started_at = Some(current.to_owned());
            write_lifecycle_state(lc_ctx.client, lc_ctx.container_id, remote_user, &lc_state).await;
        }
    }
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

    // ── phase_skips unit tests ────────────────────────────────────────────────

    #[test]
    fn phase_skips_all_done_skips_create_phases() {
        // oncreate_done=true, content unchanged → only postStart/postAttach run.
        let s = phase_skips(true, false, false);
        assert!(!s.run_oncreate, "onCreate must be skipped when done");
        assert!(
            !s.run_update_content,
            "updateContent skipped when unchanged"
        );
        assert!(!s.run_post_create, "postCreate skipped when unchanged");
    }

    #[test]
    fn phase_skips_oncreate_not_done_runs_oncreate() {
        let s = phase_skips(false, false, false);
        assert!(s.run_oncreate, "onCreate must run when not done");
        assert!(!s.run_update_content, "updateContent skipped if unchanged");
        assert!(!s.run_post_create, "postCreate skipped if unchanged");
    }

    #[test]
    fn phase_skips_content_changed_runs_content_phases() {
        let s = phase_skips(true, true, false);
        assert!(!s.run_oncreate, "onCreate still skipped when done");
        assert!(s.run_update_content, "updateContent runs when changed");
        assert!(s.run_post_create, "postCreate runs when changed");
    }

    #[test]
    fn phase_skips_nothing_done_runs_all() {
        let s = phase_skips(false, true, false);
        assert!(s.run_oncreate);
        assert!(s.run_update_content);
        assert!(s.run_post_create);
    }

    #[test]
    fn phase_skips_prebuild_forces_update_content() {
        // Matches `up` + official `rerun = !!prebuild`: prebuild re-runs
        // updateContent even when unchanged. postCreate is unreachable under
        // prebuild (flow returns STATUS_PREBUILD first), so it stays content-gated.
        let s = phase_skips(true, false, true);
        assert!(s.run_update_content, "prebuild force-runs updateContent");
        assert!(!s.run_post_create, "postCreate not forced by prebuild");
    }

    #[test]
    fn run_all_runs_every_gated_phase() {
        // The recovery path (previous background lifecycle failed) re-runs every
        // gated phase regardless of oncreate_done / content-hash.
        let s = PhaseSkips::run_all();
        assert!(s.run_oncreate, "recovery re-runs onCreate");
        assert!(s.run_update_content, "recovery re-runs updateContent");
        assert!(s.run_post_create, "recovery re-runs postCreate");
    }

    #[test]
    fn should_run_post_start_cases() {
        // Skip ONLY on a confirmed match of two Some values (no restart).
        assert!(
            !should_run_post_start(Some("T"), Some("T")),
            "no restart → skip"
        );
        assert!(
            should_run_post_start(Some("T1"), Some("T2")),
            "restart → run"
        );
        assert!(
            should_run_post_start(None, Some("T")),
            "never recorded → run"
        );
        assert!(
            should_run_post_start(Some("T"), None),
            "current unavailable → run"
        );
        assert!(should_run_post_start(None, None), "both absent → run");
    }

    // ── mock-backend integration tests ───────────────────────────────────────
    //
    // These exercise run_user_commands against a mock ContainerBackend that
    // controls the `oncreate_done` and `content_hash` responses and records
    // every exec'd command so assertions can verify which phases ran.

    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use cella_backend::traits::Platform;
    use cella_backend::types::{BuildOptions, FileToUpload, ImageDetails, InteractiveExecOptions};
    use cella_backend::{
        BackendCapabilities, BackendError, BackendKind, BoxFuture, ContainerInfo, ExecOptions,
        ExecResult, SecretMasker,
    };

    /// A minimal mock backend for lifecycle gating tests.
    ///
    /// - `exec_command` calls that look like `cat /tmp/.cella/lifecycle_state.json`
    ///   return `{"oncreate_done":true/false}` per `oncreate_done`.
    /// - Calls that look like `cat /tmp/.cella/content_hash` return `stored_hash`.
    /// - All other `exec_command` calls succeed (exit 0, empty output) and their
    ///   third `cmd` element (the sh `-c` argument) is pushed into `recorded`.
    struct LifecycleMockBackend {
        oncreate_done: bool,
        stored_hash: String,
        /// Returned for `cat /tmp/.cella/lifecycle_status.json`. Empty = absent
        /// (the common case: nothing failed). Set to `{"status":"failed"}` to
        /// exercise the recovery path.
        lifecycle_status: String,
        /// The `started_at` recorded in `lifecycle_state.json`. `None` = not yet
        /// recorded (postStart always runs). Set to a timestamp to exercise the
        /// restart-skip path.
        recorded_started_at: Option<String>,
        recorded: Arc<Mutex<Vec<String>>>,
    }

    impl LifecycleMockBackend {
        fn new(
            oncreate_done: bool,
            stored_hash: impl Into<String>,
        ) -> (Self, Arc<Mutex<Vec<String>>>) {
            let recorded = Arc::new(Mutex::new(Vec::new()));
            let backend = Self {
                oncreate_done,
                stored_hash: stored_hash.into(),
                lifecycle_status: String::new(),
                recorded_started_at: None,
                recorded: Arc::clone(&recorded),
            };
            (backend, recorded)
        }
    }

    impl ContainerBackend for LifecycleMockBackend {
        fn kind(&self) -> BackendKind {
            BackendKind::Docker
        }

        fn capabilities(&self) -> BackendCapabilities {
            BackendCapabilities {
                compose: false,
                managed_agent: false,
            }
        }

        fn find_container<'a>(
            &'a self,
            _: &'a Path,
        ) -> BoxFuture<'a, Result<Option<ContainerInfo>, BackendError>> {
            Box::pin(async { Ok(None) })
        }

        fn find_container_by_label<'a>(
            &'a self,
            _: &'a str,
        ) -> BoxFuture<'a, Result<Option<ContainerInfo>, BackendError>> {
            Box::pin(async { Ok(None) })
        }

        fn create_container<'a>(
            &'a self,
            _: &'a cella_backend::CreateContainerOptions,
        ) -> BoxFuture<'a, Result<String, BackendError>> {
            unimplemented!()
        }

        fn start_container<'a>(&'a self, _: &'a str) -> BoxFuture<'a, Result<(), BackendError>> {
            unimplemented!()
        }

        fn stop_container<'a>(&'a self, _: &'a str) -> BoxFuture<'a, Result<(), BackendError>> {
            unimplemented!()
        }

        fn remove_container<'a>(
            &'a self,
            _: &'a str,
            _: bool,
        ) -> BoxFuture<'a, Result<(), BackendError>> {
            unimplemented!()
        }

        fn inspect_container<'a>(
            &'a self,
            _: &'a str,
        ) -> BoxFuture<'a, Result<ContainerInfo, BackendError>> {
            unimplemented!()
        }

        fn list_cella_containers(
            &self,
            _: bool,
        ) -> BoxFuture<'_, Result<Vec<ContainerInfo>, BackendError>> {
            unimplemented!()
        }

        fn find_compose_service<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
        ) -> BoxFuture<'a, Result<Option<ContainerInfo>, BackendError>> {
            unimplemented!()
        }

        fn container_logs<'a>(
            &'a self,
            _: &'a str,
            _: u32,
        ) -> BoxFuture<'a, Result<String, BackendError>> {
            unimplemented!()
        }

        fn exec_command<'a>(
            &'a self,
            _container_id: &'a str,
            opts: &'a ExecOptions,
        ) -> BoxFuture<'a, Result<ExecResult, BackendError>> {
            // Intercept cella's two state-read commands and return the mock values.
            let stdout = if opts
                .cmd
                .get(1)
                .is_some_and(|a| a == "/tmp/.cella/lifecycle_state.json")
            {
                let flag = if self.oncreate_done { "true" } else { "false" };
                self.recorded_started_at.as_ref().map_or_else(
                    || format!(r#"{{"oncreate_done":{flag}}}"#),
                    |ts| format!(r#"{{"oncreate_done":{flag},"started_at":"{ts}"}}"#),
                )
            } else if opts
                .cmd
                .get(1)
                .is_some_and(|a| a == "/tmp/.cella/content_hash")
            {
                self.stored_hash.clone()
            } else if opts
                .cmd
                .get(1)
                .is_some_and(|a| a == "/tmp/.cella/lifecycle_status.json")
            {
                self.lifecycle_status.clone()
            } else {
                // Real lifecycle command: record the sh -c argument (index 2) or
                // the full command joined, so assertions can match by keyword.
                let script = opts
                    .cmd
                    .get(2)
                    .cloned()
                    .unwrap_or_else(|| opts.cmd.join(" "));
                self.recorded.lock().expect("mutex").push(script);
                String::new()
            };
            Box::pin(async move {
                Ok(ExecResult {
                    exit_code: 0,
                    stdout,
                    stderr: String::new(),
                })
            })
        }

        fn exec_stream<'a>(
            &'a self,
            container_id: &'a str,
            opts: &'a ExecOptions,
            _stdout_writer: Box<dyn std::io::Write + Send + 'a>,
            _stderr_writer: Box<dyn std::io::Write + Send + 'a>,
        ) -> BoxFuture<'a, Result<ExecResult, BackendError>> {
            // Delegate to exec_command so recording works on the non-is_text path too.
            self.exec_command(container_id, opts)
        }

        fn exec_interactive<'a>(
            &'a self,
            _: &'a str,
            _: &'a InteractiveExecOptions,
        ) -> BoxFuture<'a, Result<i64, BackendError>> {
            unimplemented!()
        }

        fn exec_detached<'a>(
            &'a self,
            _: &'a str,
            _: &'a ExecOptions,
        ) -> BoxFuture<'a, Result<String, BackendError>> {
            unimplemented!()
        }

        fn pull_image<'a>(&'a self, _: &'a str) -> BoxFuture<'a, Result<(), BackendError>> {
            unimplemented!()
        }

        fn build_image<'a>(
            &'a self,
            _: &'a BuildOptions,
        ) -> BoxFuture<'a, Result<String, BackendError>> {
            unimplemented!()
        }

        fn image_exists<'a>(&'a self, _: &'a str) -> BoxFuture<'a, Result<bool, BackendError>> {
            unimplemented!()
        }

        fn tag_image<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
        ) -> BoxFuture<'a, Result<(), BackendError>> {
            unimplemented!()
        }

        fn inspect_image_details<'a>(
            &'a self,
            _: &'a str,
        ) -> BoxFuture<'a, Result<ImageDetails, BackendError>> {
            unimplemented!()
        }

        fn inspect_image_env<'a>(
            &'a self,
            _: &'a str,
        ) -> BoxFuture<'a, Result<Vec<String>, BackendError>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn inspect_image_user<'a>(
            &'a self,
            _: &'a str,
        ) -> BoxFuture<'a, Result<String, BackendError>> {
            Box::pin(async { Ok("root".to_string()) })
        }

        fn upload_files<'a>(
            &'a self,
            _: &'a str,
            _: &'a [FileToUpload],
        ) -> BoxFuture<'a, Result<(), BackendError>> {
            unimplemented!()
        }

        fn ping(&self) -> BoxFuture<'_, Result<(), BackendError>> {
            Box::pin(async { Ok(()) })
        }

        fn host_gateway(&self) -> &'static str {
            "host.docker.internal"
        }

        fn detect_platform(&self) -> BoxFuture<'_, Result<Platform, BackendError>> {
            Box::pin(async {
                Ok(Platform {
                    os: "linux".to_string(),
                    arch: "amd64".to_string(),
                })
            })
        }

        fn detect_container_arch(&self) -> BoxFuture<'_, Result<String, BackendError>> {
            Box::pin(async { Ok("amd64".to_string()) })
        }

        fn ensure_network(&self) -> BoxFuture<'_, Result<(), BackendError>> {
            Box::pin(async { Ok(()) })
        }

        fn ensure_container_network<'a>(
            &'a self,
            _: &'a str,
            _: &'a Path,
        ) -> BoxFuture<'a, Result<(), BackendError>> {
            Box::pin(async { Ok(()) })
        }

        fn get_container_ip<'a>(
            &'a self,
            _: &'a str,
        ) -> BoxFuture<'a, Result<Option<String>, BackendError>> {
            Box::pin(async { Ok(None) })
        }

        fn ensure_agent_provisioned<'a>(
            &'a self,
            _version: &'a str,
            _arch: &'a str,
            _skip_checksum: bool,
        ) -> BoxFuture<'a, Result<(), BackendError>> {
            Box::pin(async { Ok(()) })
        }

        fn write_agent_addr<'a>(
            &'a self,
            _container_id: &'a str,
            _addr: &'a str,
            _token: &'a str,
        ) -> BoxFuture<'a, Result<(), BackendError>> {
            Box::pin(async { Ok(()) })
        }

        fn agent_volume_mount(&self) -> (String, String, bool) {
            (String::new(), String::new(), true)
        }

        fn prune_old_agent_versions<'a>(
            &'a self,
            _current_version: &'a str,
        ) -> BoxFuture<'a, Result<(), BackendError>> {
            Box::pin(async { Ok(()) })
        }
    }

    /// Build a default (no-gating) `Gating` suitable for the phase-skip integration tests.
    fn no_gating() -> Gating {
        Gating {
            stop: StopAfter {
                skip_non_blocking: false,
                prebuild: false,
            },
            stop_for_personalization: false,
            skip_post_attach: true, // skip postAttach — not relevant to our assertions
            wait_for: WaitForPhase::UpdateContent,
        }
    }

    /// Build a `RunUserCommandsInput` with distinct echo commands for every phase
    /// so each phase's execution is independently verifiable.
    fn input_with_phases<'a>(
        config: &'a Value,
        workspace_root: Option<&'a Path>,
    ) -> RunUserCommandsInput<'a> {
        RunUserCommandsInput {
            config,
            metadata: None,
            gating: no_gating(),
            dotfiles: DotfilesInputs {
                repository: None,
                install_command: None,
                target_path: "/root/dotfiles",
            },
            workspace_root,
            // No recorded started_at in these mocks → postStart runs (the
            // restart-skip cases use bespoke inputs below).
            container_started_at: None,
        }
    }

    /// Build a `LifecycleContext` backed by the given mock.
    fn lc_ctx<'a>(backend: &'a LifecycleMockBackend, env: &'a [String]) -> LifecycleContext<'a> {
        LifecycleContext {
            client: backend,
            container_id: "test-container",
            user: Some("root"),
            env,
            working_dir: None,
            is_text: false,
            on_output: None,
            secret_masker: SecretMasker::new(&[]),
        }
    }

    /// Create a `ProgressSender` that discards all events (for tests).
    fn noop_sender() -> ProgressSender {
        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        ProgressSender::new(tx, false)
    }

    /// Devcontainer config carrying a distinct echo marker for each phase.
    fn phase_config() -> Value {
        json!({
            "onCreateCommand": "MARKER_ONCREATE",
            "updateContentCommand": "MARKER_UPDATE",
            "postCreateCommand": "MARKER_POSTCREATE",
            "postStartCommand": "MARKER_POSTSTART",
            "postAttachCommand": "MARKER_POSTATTACH",
        })
    }

    #[tokio::test]
    async fn all_done_skips_oncreate_and_content_phases() {
        // (a) oncreate_done=true + content hash matches → only postStart ran
        //     (postAttach is gated off by skip_post_attach=true in no_gating()).
        let tmp = tempfile::tempdir().expect("tempdir");
        // The mock returns a hash matching what content_hash::compute computes
        // on the (empty) temp dir.
        let expected_hash = cella_git::content_hash::compute(tmp.path());

        let (backend, recorded) = LifecycleMockBackend::new(true, expected_hash);
        let config = phase_config();
        let ctx = lc_ctx(&backend, &[]);
        let input = input_with_phases(&config, Some(tmp.path()));

        let sender = noop_sender();
        let status = run_user_commands(&ctx, &input, &sender).await.expect("run");
        assert_eq!(status, STATUS_DONE);

        let cmds = recorded.lock().expect("mutex").clone();
        assert!(
            !cmds.iter().any(|c| c.contains("MARKER_ONCREATE")),
            "onCreate must be skipped; got {cmds:?}"
        );
        assert!(
            !cmds.iter().any(|c| c.contains("MARKER_UPDATE")),
            "updateContent must be skipped; got {cmds:?}"
        );
        assert!(
            !cmds.iter().any(|c| c.contains("MARKER_POSTCREATE")),
            "postCreate must be skipped; got {cmds:?}"
        );
        assert!(
            cmds.iter().any(|c| c.contains("MARKER_POSTSTART")),
            "postStart must still run; got {cmds:?}"
        );
    }

    #[tokio::test]
    async fn oncreate_not_done_runs_oncreate() {
        // (b) oncreate_done=false → onCreate runs; content unchanged → content phases skipped.
        let tmp = tempfile::tempdir().expect("tempdir");
        let expected_hash = cella_git::content_hash::compute(tmp.path());

        let (backend, recorded) = LifecycleMockBackend::new(false, expected_hash);
        let config = phase_config();
        let ctx = lc_ctx(&backend, &[]);
        let input = input_with_phases(&config, Some(tmp.path()));

        let sender = noop_sender();
        run_user_commands(&ctx, &input, &sender).await.expect("run");

        let cmds = recorded.lock().expect("mutex").clone();
        assert!(
            cmds.iter().any(|c| c.contains("MARKER_ONCREATE")),
            "onCreate must run when not done; got {cmds:?}"
        );
        assert!(
            !cmds.iter().any(|c| c.contains("MARKER_UPDATE")),
            "updateContent must be skipped (unchanged); got {cmds:?}"
        );
        assert!(
            !cmds.iter().any(|c| c.contains("MARKER_POSTCREATE")),
            "postCreate must be skipped (unchanged); got {cmds:?}"
        );
        assert!(
            cmds.iter()
                .any(|c| c.contains("> /tmp/.cella/lifecycle_state.json")),
            "oncreate_done must be persisted after onCreate runs; got {cmds:?}"
        );
    }

    #[tokio::test]
    async fn hash_mismatch_runs_content_phases() {
        // (c) oncreate_done=true, hash mismatch → updateContent + postCreate run.
        let tmp = tempfile::tempdir().expect("tempdir");

        let (backend, recorded) = LifecycleMockBackend::new(true, "stale-hash-value");
        let config = phase_config();
        let ctx = lc_ctx(&backend, &[]);
        let input = input_with_phases(&config, Some(tmp.path()));

        let sender = noop_sender();
        run_user_commands(&ctx, &input, &sender).await.expect("run");

        let cmds = recorded.lock().expect("mutex").clone();
        assert!(
            !cmds.iter().any(|c| c.contains("MARKER_ONCREATE")),
            "onCreate must be skipped when done; got {cmds:?}"
        );
        assert!(
            cmds.iter().any(|c| c.contains("MARKER_UPDATE")),
            "updateContent must run on hash mismatch; got {cmds:?}"
        );
        assert!(
            cmds.iter().any(|c| c.contains("MARKER_POSTCREATE")),
            "postCreate must run on hash mismatch; got {cmds:?}"
        );
        assert!(
            cmds.iter()
                .any(|c| c.contains("> /tmp/.cella/content_hash")),
            "new content hash must be persisted after postCreate runs; got {cmds:?}"
        );
    }

    #[tokio::test]
    async fn no_workspace_root_runs_content_phases() {
        // workspace_root=None defaults to content_changed=true → content phases run.
        let (backend, recorded) = LifecycleMockBackend::new(true, "any-hash");
        let config = phase_config();
        let ctx = lc_ctx(&backend, &[]);
        let input = input_with_phases(&config, None);

        let sender = noop_sender();
        run_user_commands(&ctx, &input, &sender).await.expect("run");

        let cmds = recorded.lock().expect("mutex").clone();
        assert!(
            cmds.iter().any(|c| c.contains("MARKER_UPDATE")),
            "updateContent must run when workspace_root is None; got {cmds:?}"
        );
        assert!(
            cmds.iter().any(|c| c.contains("MARKER_POSTCREATE")),
            "postCreate must run when workspace_root is None; got {cmds:?}"
        );
    }

    #[tokio::test]
    async fn prebuild_forces_update_content_then_stops() {
        // (d) oncreate_done=true + content unchanged + prebuild=true: onCreate
        // skipped, updateContent force-runs (official `rerun = !!prebuild`), then
        // the flow returns STATUS_PREBUILD before postCreate. Mirrors `up`.
        let tmp = tempfile::tempdir().expect("tempdir");
        let expected_hash = cella_git::content_hash::compute(tmp.path());

        let (backend, recorded) = LifecycleMockBackend::new(true, expected_hash);
        let config = phase_config();
        let ctx = lc_ctx(&backend, &[]);
        let mut input = input_with_phases(&config, Some(tmp.path()));
        input.gating.stop.prebuild = true;

        let sender = noop_sender();
        let status = run_user_commands(&ctx, &input, &sender).await.expect("run");
        assert_eq!(status, STATUS_PREBUILD, "prebuild returns its status");

        let cmds = recorded.lock().expect("mutex").clone();
        assert!(
            !cmds.iter().any(|c| c.contains("MARKER_ONCREATE")),
            "onCreate skipped (done); got {cmds:?}"
        );
        assert!(
            cmds.iter().any(|c| c.contains("MARKER_UPDATE")),
            "prebuild force-runs updateContent even when unchanged; got {cmds:?}"
        );
        assert!(
            !cmds.iter().any(|c| c.contains("MARKER_POSTCREATE")),
            "postCreate unreachable under prebuild; got {cmds:?}"
        );
    }

    #[tokio::test]
    async fn failed_background_lifecycle_recovers_all_phases() {
        // (e) lifecycle_status=failed re-runs EVERY gated phase even with
        // oncreate_done=true + content unchanged — the recovery path. (The
        // absent-status → still-skip case is covered by
        // `all_done_skips_oncreate_and_content_phases`, where the mock defaults
        // lifecycle_status to empty.)
        let tmp = tempfile::tempdir().expect("tempdir");
        let expected_hash = cella_git::content_hash::compute(tmp.path());

        let (mut backend, recorded) = LifecycleMockBackend::new(true, expected_hash);
        backend.lifecycle_status = r#"{"status":"failed"}"#.to_string();
        let config = phase_config();
        let ctx = lc_ctx(&backend, &[]);
        let input = input_with_phases(&config, Some(tmp.path()));

        let sender = noop_sender();
        run_user_commands(&ctx, &input, &sender).await.expect("run");

        let cmds = recorded.lock().expect("mutex").clone();
        assert!(
            cmds.iter().any(|c| c.contains("MARKER_ONCREATE")),
            "recovery re-runs onCreate; got {cmds:?}"
        );
        assert!(
            cmds.iter().any(|c| c.contains("MARKER_UPDATE")),
            "recovery re-runs updateContent; got {cmds:?}"
        );
        assert!(
            cmds.iter().any(|c| c.contains("MARKER_POSTCREATE")),
            "recovery re-runs postCreate; got {cmds:?}"
        );
    }

    #[tokio::test]
    async fn poststart_skipped_when_container_not_restarted() {
        // recorded started_at == current → no restart → postStart skipped.
        let tmp = tempfile::tempdir().expect("tempdir");
        let expected_hash = cella_git::content_hash::compute(tmp.path());
        let (mut backend, recorded) = LifecycleMockBackend::new(true, expected_hash);
        backend.recorded_started_at = Some("2026-06-15T12:00:00Z".to_owned());
        let config = phase_config();
        let ctx = lc_ctx(&backend, &[]);
        let mut input = input_with_phases(&config, Some(tmp.path()));
        input.container_started_at = Some("2026-06-15T12:00:00Z");

        let sender = noop_sender();
        run_user_commands(&ctx, &input, &sender).await.expect("run");

        let cmds = recorded.lock().expect("mutex").clone();
        assert!(
            !cmds.iter().any(|c| c.contains("MARKER_POSTSTART")),
            "postStart skipped when container has not restarted; got {cmds:?}"
        );
    }

    #[tokio::test]
    async fn poststart_runs_and_persists_when_restarted() {
        // recorded started_at != current → restart → postStart runs and records
        // the new started_at (mirror of up).
        let tmp = tempfile::tempdir().expect("tempdir");
        let expected_hash = cella_git::content_hash::compute(tmp.path());
        let (mut backend, recorded) = LifecycleMockBackend::new(true, expected_hash);
        backend.recorded_started_at = Some("2026-06-15T12:00:00Z".to_owned());
        let config = phase_config();
        let ctx = lc_ctx(&backend, &[]);
        let mut input = input_with_phases(&config, Some(tmp.path()));
        input.container_started_at = Some("2026-06-15T18:00:00Z");

        let sender = noop_sender();
        run_user_commands(&ctx, &input, &sender).await.expect("run");

        let cmds = recorded.lock().expect("mutex").clone();
        assert!(
            cmds.iter().any(|c| c.contains("MARKER_POSTSTART")),
            "postStart runs after a restart; got {cmds:?}"
        );
        assert!(
            cmds.iter()
                .any(|c| c.contains("> /tmp/.cella/lifecycle_state.json")),
            "started_at persisted after postStart runs; got {cmds:?}"
        );
    }
}
