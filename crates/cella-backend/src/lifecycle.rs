//! Lifecycle command parsing and execution.
//!
//! Moved from `cella-docker` so that `cella-orchestrator` can use these
//! types and functions without depending on a concrete backend crate.

use std::io;

use serde_json::Value;
use sha2::{Digest, Sha256};
use tracing::{debug, info};

use crate::error::BackendError;
use crate::progress::{ProgressSender, format_elapsed};
use crate::secret_mask::SecretMasker;
use crate::traits::ContainerBackend;
use crate::types::{ExecOptions, ExecResult};

/// Callback applied to lifecycle entries after resolution, before execution.
pub type PostResolveFn = dyn Fn(&mut Vec<cella_features::LifecycleEntry>) + Send + Sync;

/// Parsed lifecycle command.
pub enum ParsedLifecycle {
    /// Sequential commands.
    Sequential(Vec<Vec<String>>),
    /// Named commands to run in parallel.
    Parallel(Vec<(String, Vec<String>)>),
}

/// Parse a lifecycle command value into executable commands.
///
/// Handles: string → shell command, array → direct command, object → parallel named commands.
pub fn parse_lifecycle_command(value: &Value) -> ParsedLifecycle {
    match value {
        Value::String(s) => {
            ParsedLifecycle::Sequential(vec![vec!["sh".to_string(), "-c".to_string(), s.clone()]])
        }
        Value::Array(arr) => {
            let cmd: Vec<String> = arr
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect();
            ParsedLifecycle::Sequential(vec![cmd])
        }
        Value::Object(map) => {
            let commands: Vec<(String, Vec<String>)> = map
                .iter()
                .map(|(name, v)| {
                    let cmd = match v {
                        Value::String(s) => {
                            vec!["sh".to_string(), "-c".to_string(), s.clone()]
                        }
                        Value::Array(arr) => arr
                            .iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect(),
                        _ => vec!["sh".to_string(), "-c".to_string(), v.to_string()],
                    };
                    (name.clone(), cmd)
                })
                .collect();
            ParsedLifecycle::Parallel(commands)
        }
        _ => ParsedLifecycle::Sequential(vec![]),
    }
}

/// Callback type for routing lifecycle output through a progress system.
pub type OutputCallback<'a> = Box<dyn Fn(&str) + Send + Sync + 'a>;

/// Shared container context for lifecycle phase execution.
pub struct LifecycleContext<'a> {
    /// Container backend (trait object).
    pub client: &'a dyn ContainerBackend,
    /// Container to run commands in.
    pub container_id: &'a str,
    /// User to run commands as.
    pub user: Option<&'a str>,
    /// Environment variables.
    pub env: &'a [String],
    /// Working directory inside the container.
    pub working_dir: Option<&'a str>,
    /// Whether to print progress and stream output to stderr.
    pub is_text: bool,
    /// Optional callback for routing output lines through a progress system.
    ///
    /// When set, sequential lifecycle output is written through this callback
    /// (e.g., indented under an active spinner) instead of directly to stderr.
    pub on_output: Option<OutputCallback<'a>>,
    /// Masker applied to all lifecycle output before it reaches any sink.
    ///
    /// Built from `--secrets-file` `KEY=VALUE` entries. When no secrets are
    /// configured the masker is a cheap no-op passthrough.
    pub secret_masker: SecretMasker,
}

/// A `Write` adapter that buffers lines, masks secret values, and forwards
/// each complete line through a callback with indentation.
struct CallbackWriter<'a> {
    callback: &'a (dyn Fn(&str) + Send + Sync),
    masker: &'a SecretMasker,
    buf: Vec<u8>,
}

impl<'a> CallbackWriter<'a> {
    fn new(callback: &'a (dyn Fn(&str) + Send + Sync), masker: &'a SecretMasker) -> Self {
        Self {
            callback,
            masker,
            buf: Vec::with_capacity(256),
        }
    }

    fn flush_lines(&mut self) {
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let line = String::from_utf8_lossy(&self.buf[..pos]);
            if !line.trim().is_empty() {
                let masked = self.masker.mask(&line);
                (self.callback)(&format!("      {masked}"));
            }
            self.buf.drain(..=pos);
        }
    }

    fn flush_remaining(&mut self) {
        if !self.buf.is_empty() {
            let line = String::from_utf8_lossy(&self.buf);
            if !line.trim().is_empty() {
                let masked = self.masker.mask(&line);
                (self.callback)(&format!("      {masked}"));
            }
            self.buf.clear();
        }
    }
}

impl io::Write for CallbackWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(buf);
        self.flush_lines();
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flush_remaining();
        Ok(())
    }
}

impl Drop for CallbackWriter<'_> {
    fn drop(&mut self) {
        self.flush_remaining();
    }
}

/// A `Write` adapter that buffers lines, masks secret values, and writes
/// each complete line to the wrapped sink.
///
/// Used for the stderr fallback path in `run_sequential` so that no byte
/// escapes to stderr unmasked.
struct MaskingWriter<W: io::Write> {
    inner: W,
    masker: SecretMasker,
    buf: Vec<u8>,
}

impl<W: io::Write> MaskingWriter<W> {
    fn new(inner: W, masker: SecretMasker) -> Self {
        Self {
            inner,
            masker,
            buf: Vec::with_capacity(256),
        }
    }

    fn flush_lines(&mut self) -> io::Result<()> {
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let line = String::from_utf8_lossy(&self.buf[..pos]);
            let masked = self.masker.mask(&line);
            self.inner.write_all(masked.as_bytes())?;
            self.inner.write_all(b"\n")?;
            self.buf.drain(..=pos);
        }
        Ok(())
    }

    fn flush_remaining(&mut self) -> io::Result<()> {
        if !self.buf.is_empty() {
            let line = String::from_utf8_lossy(&self.buf);
            let masked = self.masker.mask(&line);
            self.inner.write_all(masked.as_bytes())?;
            self.buf.clear();
        }
        Ok(())
    }
}

impl<W: io::Write> io::Write for MaskingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(buf);
        self.flush_lines()?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flush_remaining()?;
        self.inner.flush()
    }
}

impl<W: io::Write> Drop for MaskingWriter<W> {
    fn drop(&mut self) {
        // Best-effort flush on drop; ignore errors (same pattern as BufWriter).
        let _ = self.flush_remaining();
    }
}

/// Run sequential lifecycle commands, streaming output when `is_text`.
async fn run_sequential(
    ctx: &LifecycleContext<'_>,
    phase: &str,
    commands: Vec<Vec<String>>,
) -> Result<(), BackendError> {
    for cmd in commands {
        if cmd.is_empty() {
            continue;
        }
        debug!("{phase}: {}", cmd.join(" "));
        let opts = ExecOptions {
            cmd,
            user: ctx.user.map(String::from),
            env: Some(ctx.env.to_vec()),
            working_dir: ctx.working_dir.map(String::from),
        };
        let result = if ctx.is_text {
            if let Some(ref on_output) = ctx.on_output {
                // Route through progress system with indentation; mask secrets.
                ctx.client
                    .exec_stream(
                        ctx.container_id,
                        &opts,
                        Box::new(CallbackWriter::new(on_output.as_ref(), &ctx.secret_masker)),
                        Box::new(CallbackWriter::new(on_output.as_ref(), &ctx.secret_masker)),
                    )
                    .await?
            } else {
                // Fallback: stream to stderr through masking writer so no
                // secret bytes reach the terminal unmasked.
                ctx.client
                    .exec_stream(
                        ctx.container_id,
                        &opts,
                        Box::new(MaskingWriter::new(io::stderr(), ctx.secret_masker.clone())),
                        Box::new(MaskingWriter::new(io::stderr(), ctx.secret_masker.clone())),
                    )
                    .await?
            }
        } else {
            ctx.client.exec_command(ctx.container_id, &opts).await?
        };

        check_exit_code(&result, phase, None, &ctx.secret_masker)?;
    }
    Ok(())
}

/// Run named lifecycle commands in parallel, letting every command finish
/// before surfacing a failure.
///
/// Matches the official runner's `Promise.allSettled` semantics: all named
/// commands run to completion even if a sibling fails, then the first error
/// (in command order) is returned. Using `try_join_all` here would cancel the
/// remaining in-flight commands on the first failure, leaving parallel setup
/// steps half-done.
async fn run_parallel(
    ctx: &LifecycleContext<'_>,
    phase: &str,
    commands: Vec<(String, Vec<String>)>,
) -> Result<(), BackendError> {
    let mut futures = Vec::new();
    for (name, cmd) in commands {
        let user = ctx.user.map(String::from);
        let env = ctx.env.to_vec();
        let working_dir = ctx.working_dir.map(String::from);
        let phase = phase.to_string();
        let container_id = ctx.container_id.to_string();

        futures.push(async move {
            debug!("{phase} [{name}]: {}", cmd.join(" "));
            let result = ctx
                .client
                .exec_command(
                    &container_id,
                    &ExecOptions {
                        cmd,
                        user,
                        env: Some(env),
                        working_dir,
                    },
                )
                .await?;

            check_exit_code(&result, &phase, Some(&name), &ctx.secret_masker)?;
            Ok::<ExecResult, BackendError>(result)
        });
    }

    // Wait for ALL commands to finish (allSettled semantics), then surface
    // the first error in command order. Print output from successful commands
    // regardless of whether a sibling failed.
    let settled = futures_util::future::join_all(futures).await;
    let mut first_err: Option<BackendError> = None;
    let mut results = Vec::with_capacity(settled.len());
    for outcome in settled {
        match outcome {
            Ok(r) => results.push(r),
            Err(e) => {
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
        }
    }

    if ctx.is_text {
        print_completed_output(&results, &ctx.secret_masker);
    }

    first_err.map_or(Ok(()), Err)
}

/// Check an exec result exit code, returning `LifecycleFailed` on non-zero.
fn check_exit_code(
    result: &ExecResult,
    phase: &str,
    name: Option<&str>,
    masker: &SecretMasker,
) -> Result<(), BackendError> {
    if result.exit_code != 0 {
        let prefix = name.map_or(String::new(), |n| format!("[{n}] "));
        // The failing command's stderr is embedded in the error message, which
        // surfaces in the JSON error envelope — mask secrets before it does.
        return Err(BackendError::LifecycleFailed {
            phase: phase.to_string(),
            message: format!(
                "{prefix}exit code {}: {}",
                result.exit_code,
                masker.mask(result.stderr.trim())
            ),
        });
    }
    Ok(())
}

/// Print stdout/stderr from completed parallel exec results to stderr, masking
/// secret values before any bytes reach the terminal.
fn print_completed_output(results: &[ExecResult], masker: &SecretMasker) {
    for exec_result in results {
        if !exec_result.stdout.is_empty() {
            eprint!("{}", masker.mask(&exec_result.stdout));
        }
        if !exec_result.stderr.is_empty() {
            eprint!("{}", masker.mask(&exec_result.stderr));
        }
    }
}

/// Execute lifecycle commands for a phase.
///
/// When `ctx.is_text` is true, prints origin-tracked progress (matching the original
/// devcontainer CLI phrasing) and streams sequential command output to stderr.
///
/// # Errors
///
/// Returns `BackendError::LifecycleFailed` if any command fails.
pub async fn run_lifecycle_phase(
    ctx: &LifecycleContext<'_>,
    phase: &str,
    value: &Value,
    origin: &str,
) -> Result<(), BackendError> {
    info!("Running the {phase} from {origin}...");
    debug!("Running {phase} from {origin}");

    match parse_lifecycle_command(value) {
        ParsedLifecycle::Sequential(commands) => run_sequential(ctx, phase, commands).await?,
        ParsedLifecycle::Parallel(commands) => run_parallel(ctx, phase, commands).await?,
    }

    debug!("{phase} completed");
    Ok(())
}

// ---------------------------------------------------------------------------
// Lifecycle phase management (merged from cella-orchestrator)
// ---------------------------------------------------------------------------

/// Shell-quote an argv array into a single command string safe for `sh -c`.
fn shell_quote_argv(argv: &[&str]) -> String {
    argv.iter()
        .map(|a| {
            if a.chars()
                .all(|c| c.is_alphanumeric() || "-_/.:=@".contains(c))
            {
                (*a).to_string()
            } else {
                format!("'{}'", a.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Tracks which lifecycle phases have already run inside a container.
///
/// Stored at `/tmp/.cella/lifecycle_state.json` so that restarts of prebuilt
/// containers can skip phases that already completed.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct LifecycleState {
    #[serde(default)]
    pub oncreate_done: bool,
    /// The container's `started_at` when `run-user-commands` last ran
    /// `postStartCommand`. Lets a later `run-user-commands` skip postStart when
    /// the container has not restarted. Written ONLY by `run-user-commands` (the
    /// single writer — `up` deliberately doesn't touch it, which avoids a
    /// foreground/background write race on this file).
    #[serde(default)]
    pub started_at: Option<String>,
}

/// Read the persisted lifecycle state from a running container.
pub async fn read_lifecycle_state(
    client: &dyn ContainerBackend,
    container_id: &str,
    user: &str,
) -> LifecycleState {
    let result = client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: vec![
                    "cat".to_string(),
                    "/tmp/.cella/lifecycle_state.json".to_string(),
                ],
                user: Some(user.to_string()),
                env: None,
                working_dir: None,
            },
        )
        .await;

    result
        .ok()
        .filter(|r| r.exit_code == 0)
        .and_then(|r| serde_json::from_str(r.stdout.trim()).ok())
        .unwrap_or_default()
}

/// Persist the lifecycle state inside a running container.
pub async fn write_lifecycle_state(
    client: &dyn ContainerBackend,
    container_id: &str,
    user: &str,
    state: &LifecycleState,
) {
    let json = serde_json::to_string(state).unwrap_or_default();
    let _ = client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: vec![
                    "sh".to_string(),
                    "-c".to_string(),
                    format!(
                        "mkdir -p /tmp/.cella && printf '%s' '{json}' > /tmp/.cella/lifecycle_state.json"
                    ),
                ],
                user: Some(user.to_string()),
                env: None,
                working_dir: None,
            },
        )
        .await;
}

/// Compute a deterministic hash of lifecycle command entries for a given phase.
pub fn hash_lifecycle_entries(entries: &[cella_features::LifecycleEntry]) -> String {
    let mut hasher = Sha256::new();
    for entry in entries {
        hasher.update(entry.origin.as_bytes());
        hasher.update(b":");
        hasher.update(entry.command.to_string().as_bytes());
        hasher.update(b"\n");
    }
    hex::encode(hasher.finalize())
}

/// Resolve lifecycle entries for a phase from feature-resolved config.
pub fn resolve_phase_entries<'a>(
    resolved_features: Option<&'a cella_features::ResolvedFeatures>,
    phase: &str,
) -> &'a [cella_features::LifecycleEntry] {
    resolved_features.map_or(&[], |rf| match phase {
        "onCreateCommand" => &rf.container_config.lifecycle.on_create,
        "updateContentCommand" => &rf.container_config.lifecycle.update_content,
        "postCreateCommand" => &rf.container_config.lifecycle.post_create,
        "postStartCommand" => &rf.container_config.lifecycle.post_start,
        "postAttachCommand" => &rf.container_config.lifecycle.post_attach,
        _ => &[],
    })
}

/// Resolve lifecycle entries for a phase, supporting both feature-resolved
/// configs and the metadata label format used by existing containers.
pub fn lifecycle_entries_for_phase(
    metadata: Option<&str>,
    config: &Value,
    phase: &str,
) -> Vec<cella_features::LifecycleEntry> {
    metadata.map_or_else(
        || {
            config
                .get(phase)
                .filter(|v| !v.is_null())
                .map(|cmd| {
                    vec![cella_features::LifecycleEntry {
                        origin: "devcontainer.json".into(),
                        command: cmd.clone(),
                    }]
                })
                .unwrap_or_default()
        },
        |meta_json| cella_features::lifecycle_from_metadata_label(meta_json, phase),
    )
}

/// Build the effective lifecycle-metadata array for an existing container,
/// mirroring official `getImageMetadataFromContainer`.
///
/// When the container was matched by id-labels / workspace folder (official
/// `hasIdLabels === true`), the container's own `devcontainer.metadata` alone
/// drives the lifecycle, so the metadata is returned unchanged. When it was
/// matched by raw `--container-id` (`hasIdLabels === false`), the on-disk
/// `--config`/`--override-config` is appended as the FINAL array entry —
/// official does this via `getDevcontainerMetadata` (`pick(config,
/// pickConfigProperties)`), so the config's lifecycle hooks and `waitFor` take
/// effect after the baked metadata. Only the lifecycle hooks and `waitFor` are
/// picked here; `remoteEnv` / `remoteUser` / `userEnvProbe` are layered by the
/// caller and must not be double-counted via this array.
///
/// With no metadata label the array is empty regardless of branch (official's
/// no-label branch returns `getDevcontainerMetadata([], config)`), so `None` is
/// returned and the caller sources lifecycle from the config directly.
#[must_use]
pub fn effective_lifecycle_metadata(
    metadata: Option<&str>,
    config: &Value,
    append_config: bool,
) -> Option<String> {
    let raw = metadata?;
    if !append_config {
        return Some(raw.to_string());
    }
    let Ok(mut entries) = serde_json::from_str::<Vec<Value>>(raw) else {
        return Some(raw.to_string());
    };
    let mut picked = serde_json::Map::new();
    for key in [
        "onCreateCommand",
        "updateContentCommand",
        "postCreateCommand",
        "postStartCommand",
        "postAttachCommand",
        "waitFor",
    ] {
        if let Some(v) = config.get(key).filter(|v| !v.is_null()) {
            picked.insert(key.to_string(), v.clone());
        }
    }
    if picked.is_empty() {
        return Some(raw.to_string());
    }
    entries.push(Value::Object(picked));
    serde_json::to_string(&entries).map_or_else(|_| Some(raw.to_string()), Some)
}

/// Run a sequence of origin-tracked lifecycle entries with progress tracking.
///
/// # Errors
///
/// Returns an error if any lifecycle entry's command fails.
pub async fn run_lifecycle_entries(
    ctx: &LifecycleContext<'_>,
    phase: &str,
    entries: &[cella_features::LifecycleEntry],
    progress: &ProgressSender,
) -> Result<(), BackendError> {
    for entry in entries {
        let label = format!("Running the {phase} from {}...", entry.origin);
        let start = std::time::Instant::now();
        progress.println(&format!("  \x1b[36m▸\x1b[0m {label}"));
        let result = run_lifecycle_phase(ctx, phase, &entry.command, &entry.origin).await;
        let elapsed = format_elapsed(start.elapsed());
        match &result {
            Ok(()) => progress.println(&format!("  \x1b[32m✓\x1b[0m {label}{elapsed}")),
            Err(e) => progress.println(&format!("  \x1b[31m✗\x1b[0m {label}: {e}")),
        }
        result?;
    }
    Ok(())
}

/// Resolve lifecycle entries using three-tier priority:
///
/// 1. `resolved_features` entries (from local feature build) -- if non-empty
/// 2. Image metadata entries (from prebuilt image) -- fallback
/// 3. `config.get(phase)` entries (from `devcontainer.json`) -- final fallback
fn resolve_entries_with_metadata(
    resolved_features: Option<&cella_features::ResolvedFeatures>,
    image_metadata: Option<&str>,
    config: &Value,
    phase: &str,
) -> Vec<cella_features::LifecycleEntry> {
    let feature_entries = resolve_phase_entries(resolved_features, phase);
    if !feature_entries.is_empty() {
        return feature_entries.to_vec();
    }

    if let Some(meta) = image_metadata {
        let meta_entries = cella_features::lifecycle_from_metadata_label(meta, phase);
        if !meta_entries.is_empty() {
            return meta_entries;
        }
    }

    config
        .get(phase)
        .filter(|v| !v.is_null())
        .map(|cmd| {
            vec![cella_features::LifecycleEntry {
                origin: "devcontainer.json".into(),
                command: cmd.clone(),
            }]
        })
        .unwrap_or_default()
}

/// Run all lifecycle phases for a first-create scenario.
///
/// # Errors
///
/// Returns an error if any lifecycle command fails.
pub async fn run_all_lifecycle_phases(
    lc_ctx: &LifecycleContext<'_>,
    config: &Value,
    resolved_features: Option<&cella_features::ResolvedFeatures>,
    image_metadata: Option<&str>,
    progress: &ProgressSender,
    post_resolve: Option<&PostResolveFn>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let phases = [
        "onCreateCommand",
        "updateContentCommand",
        "postCreateCommand",
        "postStartCommand",
        "postAttachCommand",
    ];

    for phase in phases {
        let mut entries =
            resolve_entries_with_metadata(resolved_features, image_metadata, config, phase);
        if let Some(f) = post_resolve {
            f(&mut entries);
        }
        run_lifecycle_entries(lc_ctx, phase, &entries, progress).await?;
    }

    Ok(())
}

/// Store the workspace content hash inside the container for future change detection.
pub async fn write_content_hash(
    client: &dyn ContainerBackend,
    container_id: &str,
    user: &str,
    workspace_root: &std::path::Path,
) {
    let hash = cella_git::content_hash::compute(workspace_root);
    let _ = client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: vec![
                    "sh".to_string(),
                    "-c".to_string(),
                    format!(
                        "mkdir -p /tmp/.cella && printf '%s' '{hash}' > /tmp/.cella/content_hash"
                    ),
                ],
                user: Some(user.to_string()),
                env: None,
                working_dir: None,
            },
        )
        .await;
}

/// Which lifecycle phase to wait for before returning from `cella up`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitForPhase {
    Initialize,
    OnCreate,
    UpdateContent,
    PostCreate,
    PostStart,
}

impl WaitForPhase {
    /// Parse from the `waitFor` config property. Default: `UpdateContentCommand`.
    pub fn from_config(config: &Value) -> Self {
        match config.get("waitFor").and_then(|v| v.as_str()) {
            Some("initializeCommand") => Self::Initialize,
            Some("onCreateCommand") => Self::OnCreate,
            Some("postCreateCommand") => Self::PostCreate,
            Some("postStartCommand") => Self::PostStart,
            _ => Self::UpdateContent,
        }
    }

    /// Resolve `waitFor` for an existing container from its effective
    /// lifecycle-metadata array (see [`effective_lifecycle_metadata`]).
    ///
    /// Mirrors official `mergeConfiguration` (`reversed.find(entry =>
    /// entry.waitFor)?.waitFor`): the LAST array entry that declares `waitFor`
    /// wins. Because the effective array already carries the on-disk `--config`
    /// as its final entry in the `--container-id` case, this yields the config's
    /// `waitFor` there and the baked metadata's otherwise. With no metadata
    /// array the value comes from `config` (official's no-label branch); absent
    /// everywhere it defaults to `updateContentCommand`.
    #[must_use]
    pub fn from_metadata_or_config(metadata: Option<&str>, config: &Value) -> Self {
        let Some(raw) = metadata else {
            return Self::from_config(config);
        };
        let entries: Vec<Value> = serde_json::from_str(raw).unwrap_or_default();
        entries
            .iter()
            .rev()
            .find(|e| e.get("waitFor").and_then(Value::as_str).is_some())
            .map_or(Self::UpdateContent, Self::from_config)
    }

    pub(crate) const fn ordinal(self) -> usize {
        match self {
            Self::Initialize => 0,
            Self::OnCreate => 1,
            Self::UpdateContent => 2,
            Self::PostCreate => 3,
            Self::PostStart => 4,
        }
    }
}

/// Error for `--expect-existing-container` when no container exists.
///
/// Matches the official devcontainer CLI exactly (including the trailing
/// period), so scripted consumers see identical output.
pub const EXPECTED_CONTAINER_MISSING: &str = "The expected container does not exist.";

/// Which "stop after this phase" flags are active for an `up`.
///
/// Both flags suppress cella's background tail: the phases past the stop point
/// are dropped entirely (not backgrounded). Modelled as a struct rather than
/// two `LifecycleGate` bools so the gate stays under the bool-count lint and
/// the stop semantics live in one place.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StopAfter {
    /// `--skip-non-blocking-commands`: stop after the resolved `waitFor` phase.
    pub skip_non_blocking: bool,
    /// `--prebuild`: stop after `onCreate` + `updateContent` (and force-rerun
    /// `updateContentCommand`).
    pub prebuild: bool,
}

impl StopAfter {
    /// Whether any stop-after flag is active (the background tail is dropped).
    #[must_use]
    pub const fn any(self) -> bool {
        self.skip_non_blocking || self.prebuild
    }
}

/// Gates which lifecycle phases run, and how, for a single `up` invocation.
///
/// Built from the devcontainer-CLI-parity lifecycle flags
/// (`--skip-post-create`, `--skip-non-blocking-commands`, `--prebuild`,
/// `--skip-post-attach`).
///
/// `Default` reproduces cella's standard behavior (everything runs; phases past
/// `wait_for` are backgrounded), so callers that pass no flags are unaffected.
///
/// Field semantics:
/// - `enabled == false` (from `--skip-post-create`): run NOTHING — no phases,
///   no dotfiles, no `userEnvProbe`.
/// - `wait_for`: the resolved `waitFor` phase (never mutated). Phases up to
///   (and not including) it run in the foreground; the rest are backgrounded
///   (default) or dropped (when a stop-after flag is active).
/// - `stop`: the active stop-after flags (`--skip-non-blocking-commands` /
///   `--prebuild`).
/// - `skip_post_attach` (from `--skip-post-attach`): drop only
///   `postAttachCommand`, regardless of where it would otherwise run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LifecycleGate {
    /// Master switch. `false` skips all phases + dotfiles + `userEnvProbe`.
    pub enabled: bool,
    /// Resolved `waitFor` phase — the default foreground/background boundary.
    pub wait_for: WaitForPhase,
    /// Active "stop after this phase" flags.
    pub stop: StopAfter,
    /// `--skip-post-attach`: drop only `postAttachCommand`.
    pub skip_post_attach: bool,
}

impl Default for LifecycleGate {
    fn default() -> Self {
        Self {
            enabled: true,
            wait_for: WaitForPhase::UpdateContent,
            stop: StopAfter::default(),
            skip_post_attach: false,
        }
    }
}

impl LifecycleGate {
    /// Build a gate from the resolved `waitFor` phase and the lifecycle flags.
    /// This is the single place flag interactions are resolved, so every
    /// consumer (create, reuse, restart, compose) honors them uniformly.
    #[must_use]
    pub const fn new(
        wait_for: WaitForPhase,
        skip_post_create: bool,
        stop: StopAfter,
        skip_post_attach: bool,
    ) -> Self {
        Self {
            enabled: !skip_post_create,
            wait_for,
            stop,
            skip_post_attach,
        }
    }

    /// Run phases past the foreground boundary in the background (cella
    /// default) vs. drop them entirely (when a stop-after flag is active).
    #[must_use]
    pub const fn background_tail(self) -> bool {
        !self.stop.any()
    }

    /// `--prebuild` force-reruns `updateContentCommand` even when the content
    /// hash is unchanged (official `injectHeadless.ts`: `rerun = !!prebuild`).
    #[must_use]
    pub const fn rerun_update_content(self) -> bool {
        self.stop.prebuild
    }

    /// Whether the named post-create phase runs at all under this gate
    /// (foreground or background). Unknown phase names return `false`.
    ///
    /// Used by callers that run phases sequentially without cella's
    /// foreground/background split (e.g. the compose path), where the only
    /// question is "does this phase run?".
    #[must_use]
    pub fn runs_phase(self, phase: &str) -> bool {
        POST_CREATE_PHASES
            .iter()
            .position(|&p| p == phase)
            .is_some_and(|i| plan_phases(self)[i] != PhaseAction::Skip)
    }

    /// Whether `postAttachCommand` runs at all under this gate. Used by the
    /// reuse paths (`handle_running`, restart, compose-already-running) where
    /// postAttach is the only deferred phase and runs on every attach.
    #[must_use]
    pub fn runs_post_attach(self) -> bool {
        self.runs_phase("postAttachCommand")
    }

    /// Whether `postStartCommand` runs at all under this gate. Used by the
    /// restart path which runs postStart + postAttach on a started container.
    #[must_use]
    pub fn runs_post_start(self) -> bool {
        self.runs_phase("postStartCommand")
    }

    /// The foreground boundary index into the canonical phase array
    /// `[onCreate, updateContent, postCreate, postStart, postAttach]`.
    /// Phases at indices `< boundary` run in the foreground.
    ///
    /// The boundary is the resolved `waitFor` ordinal, lowered to the prebuild
    /// stop point (`updateContent`) when `--prebuild` is set. When both
    /// `--prebuild` and `--skip-non-blocking-commands` are set, the EARLIER
    /// stop wins (official: `skipNonBlocking` returns before the prebuild check
    /// when its `waitFor` phase is earlier — `injectHeadless.ts` edge case).
    #[must_use]
    pub const fn foreground_boundary(self) -> usize {
        let wait_boundary = match self.wait_for {
            WaitForPhase::Initialize => 0,
            _ => self.wait_for.ordinal(),
        };
        if self.stop.prebuild {
            let prebuild_boundary = PHASE_UPDATE_CONTENT + 1;
            // skip-non-blocking can stop EARLIER than prebuild; take the min.
            // prebuild alone IGNORES waitFor, so it pins to its own boundary.
            return if self.stop.skip_non_blocking {
                if wait_boundary < prebuild_boundary {
                    wait_boundary
                } else {
                    prebuild_boundary
                }
            } else {
                prebuild_boundary
            };
        }
        wait_boundary
    }
}

/// Index of `updateContentCommand` in the canonical post-create phase array.
const PHASE_UPDATE_CONTENT: usize = 1;

/// The canonical post-create phase array used by the lifecycle runner.
const POST_CREATE_PHASES: [&str; 5] = [
    "onCreateCommand",
    "updateContentCommand",
    "postCreateCommand",
    "postStartCommand",
    "postAttachCommand",
];

/// Decision for a single phase under a [`LifecycleGate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PhaseAction {
    /// Run in the foreground (blocking) before `up` returns.
    Foreground,
    /// Defer to a detached background process after `up` returns.
    Background,
    /// Do not run at all.
    Skip,
}

/// Pure phase-selection logic: given a gate, decide what happens to each phase
/// in [`POST_CREATE_PHASES`]. This is the single source of truth the runner
/// routes through, and is unit-tested exhaustively.
pub(crate) fn plan_phases(gate: LifecycleGate) -> [PhaseAction; 5] {
    if !gate.enabled {
        return [PhaseAction::Skip; 5];
    }
    let boundary = gate.foreground_boundary();
    let mut plan = [PhaseAction::Skip; 5];
    for (i, slot) in plan.iter_mut().enumerate() {
        let is_post_attach = POST_CREATE_PHASES[i] == "postAttachCommand";
        if gate.skip_post_attach && is_post_attach {
            *slot = PhaseAction::Skip;
        } else if i < boundary {
            *slot = PhaseAction::Foreground;
        } else if gate.background_tail() {
            *slot = PhaseAction::Background;
        } else {
            *slot = PhaseAction::Skip;
        }
    }
    plan
}

/// Run lifecycle phases up to `wait_for`, then spawn remaining phases in background.
///
/// # Errors
///
/// Returns an error if any foreground lifecycle command fails.
pub async fn run_lifecycle_phases_with_wait_for(
    lc_ctx: &LifecycleContext<'_>,
    config: &Value,
    resolved_features: Option<&cella_features::ResolvedFeatures>,
    image_metadata: Option<&str>,
    progress: &ProgressSender,
    gate: LifecycleGate,
    post_resolve: Option<&PostResolveFn>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // `enabled == false` (--skip-post-create) skips every phase. Callers also
    // skip dotfiles and the userEnvProbe, so nothing in the post-create chain
    // runs.
    if !gate.enabled {
        return Ok(());
    }

    let plan = plan_phases(gate);
    let mut background_cmds: Vec<String> = Vec::new();

    for (i, &phase) in POST_CREATE_PHASES.iter().enumerate() {
        if plan[i] == PhaseAction::Skip {
            continue;
        }
        let mut entries =
            resolve_entries_with_metadata(resolved_features, image_metadata, config, phase);
        if let Some(f) = post_resolve {
            f(&mut entries);
        }

        match plan[i] {
            PhaseAction::Foreground => {
                run_lifecycle_entries(lc_ctx, phase, &entries, progress).await?;
            }
            PhaseAction::Background => {
                for entry in &entries {
                    background_cmds.push(entry_to_shell_command(entry));
                }
            }
            PhaseAction::Skip => {}
        }
    }

    if !background_cmds.is_empty() {
        let script = background_cmds.join("\n");
        let full_script = format!(
            "mkdir -p /tmp/.cella\n\
             (\n  set -e\n  {script}\n)\n\
             if [ $? -eq 0 ]; then\n\
               printf '{{\"status\":\"completed\"}}' > /tmp/.cella/lifecycle_status.json\n\
               printf '{{\"oncreate_done\":true}}' > /tmp/.cella/lifecycle_state.json\n\
             else\n\
               printf '{{\"status\":\"failed\"}}' > /tmp/.cella/lifecycle_status.json\n\
             fi\n"
        );

        debug!(
            "Spawning background lifecycle: {} commands",
            background_cmds.len()
        );
        let _ = lc_ctx
            .client
            .exec_detached(
                lc_ctx.container_id,
                &ExecOptions {
                    cmd: vec!["sh".to_string(), "-c".to_string(), full_script],
                    user: lc_ctx.user.map(String::from),
                    env: Some(lc_ctx.env.to_vec()),
                    working_dir: lc_ctx.working_dir.map(String::from),
                },
            )
            .await;
    }

    Ok(())
}

/// Convert a lifecycle entry into a shell command string for background execution.
fn entry_to_shell_command(entry: &cella_features::LifecycleEntry) -> String {
    if let Some(s) = entry.command.as_str() {
        return s.to_string();
    }
    if let Some(arr) = entry.command.as_array() {
        let cmd: Vec<&str> = arr.iter().filter_map(|v| v.as_str()).collect();
        if !cmd.is_empty() {
            return shell_quote_argv(&cmd);
        }
        return "true".to_string();
    }
    if let Some(obj) = entry.command.as_object() {
        let mut parallel = Vec::new();
        for val in obj.values() {
            if let Some(s) = val.as_str() {
                parallel.push(s.to_string());
            } else if let Some(arr) = val.as_array() {
                let cmd: Vec<&str> = arr.iter().filter_map(|v| v.as_str()).collect();
                if !cmd.is_empty() {
                    parallel.push(shell_quote_argv(&cmd));
                }
            }
        }
        if parallel.is_empty() {
            return "true".to_string();
        }
        let mut lines = Vec::new();
        lines.push("_cella_pids=\"\"".to_string());
        for cmd in &parallel {
            lines.push(format!("( {cmd} ) & _cella_pids=\"$_cella_pids $!\""));
        }
        lines.push("_cella_rc=0".to_string());
        lines.push(
            "for _cella_pid in $_cella_pids; do wait \"$_cella_pid\" || _cella_rc=1; done"
                .to_string(),
        );
        lines.push("[ \"$_cella_rc\" -eq 0 ] || exit 1".to_string());
        return lines.join("\n");
    }
    "true".to_string()
}

/// Whether the workspace content differs from the content hash stored in the container.
///
/// Returns `true` (→ run the content phases) when no hash is stored yet or when
/// the stored hash does not match the current workspace hash. This is a pure
/// reader: it never writes to the container.
pub async fn content_changed(
    lc_ctx: &LifecycleContext<'_>,
    workspace_root: &std::path::Path,
) -> bool {
    // Read the stored hash FIRST: when none is stored (first run / older
    // container) the answer is "changed" regardless, so we skip the workspace
    // scan entirely.
    let stored = lc_ctx
        .client
        .exec_command(
            lc_ctx.container_id,
            &ExecOptions {
                cmd: vec!["cat".to_string(), "/tmp/.cella/content_hash".to_string()],
                user: lc_ctx.user.map(String::from),
                env: None,
                working_dir: None,
            },
        )
        .await
        .ok()
        .filter(|r| r.exit_code == 0)
        .map(|r| r.stdout.trim().to_string());
    stored.is_none_or(|stored| cella_git::content_hash::compute(workspace_root) != stored)
}

/// Whether the container's last background lifecycle run recorded a failure.
///
/// Reads `/tmp/.cella/lifecycle_status.json`, which is written ONLY by the
/// background lifecycle chain. Returns `true` only on an explicit
/// `{"status":"failed"}`. An ABSENT file (a fully-foreground `up`, an
/// external/VS Code container seeded without it, or an older cella) means
/// nothing failed → `false`; the normal skip applies. Mirrors `up`'s own
/// `.contains("\"failed\"")` check in `handle_running`. Pure reader.
pub async fn lifecycle_failed(lc_ctx: &LifecycleContext<'_>) -> bool {
    lc_ctx
        .client
        .exec_command(
            lc_ctx.container_id,
            &ExecOptions {
                cmd: vec![
                    "cat".to_string(),
                    "/tmp/.cella/lifecycle_status.json".to_string(),
                ],
                user: lc_ctx.user.map(String::from),
                env: None,
                working_dir: None,
            },
        )
        .await
        .ok()
        .filter(|r| r.exit_code == 0)
        .is_some_and(|r| r.stdout.contains("\"failed\""))
}

/// Check for workspace content changes and re-run updateContentCommand + postCreateCommand.
///
/// # Errors
///
/// Returns an error if any re-run lifecycle command fails.
pub async fn check_and_run_content_update(
    lc_ctx: &LifecycleContext<'_>,
    config: &Value,
    metadata: Option<&str>,
    workspace_root: &std::path::Path,
    progress: &ProgressSender,
    gate: LifecycleGate,
    post_resolve: Option<&PostResolveFn>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // --skip-post-create gates the entire content-update on reuse.
    if !gate.enabled {
        return Ok(());
    }

    // postCreateCommand is index 2 in the canonical phase array. Under
    // --skip-non-blocking-commands (default waitFor=updateContent) or --prebuild
    // the boundary stops before postCreate, so on reuse we re-run only
    // updateContentCommand. plan_phases is the single source of truth.
    let plan = plan_phases(gate);
    let run_update_content = plan[1] != PhaseAction::Skip;
    let run_post_create = plan[2] != PhaseAction::Skip;
    if !run_update_content && !run_post_create {
        return Ok(());
    }

    // --prebuild force-reruns updateContentCommand even when the hash matches
    // (official injectHeadless.ts: rerun = !!prebuild). Otherwise honor the
    // content-hash short-circuit. Check rerun FIRST so prebuild skips the scan.
    if !gate.rerun_update_content() && !content_changed(lc_ctx, workspace_root).await {
        return Ok(());
    }

    let phases: &[&str] = if run_post_create {
        progress
            .println("  Content changed, re-running updateContentCommand + postCreateCommand...");
        &["updateContentCommand", "postCreateCommand"]
    } else {
        // prebuild / skip-non-blocking: updateContent only, no postCreate.
        progress.println("  Re-running updateContentCommand...");
        &["updateContentCommand"]
    };

    for &phase in phases {
        let mut entries = lifecycle_entries_for_phase(metadata, config, phase);
        // When metadata is present but defines no command for this phase, fall
        // back to the devcontainer.json command — pushed into `entries` so it
        // goes through `post_resolve` (variable substitution) like every other
        // entry, instead of being run raw.
        if entries.is_empty()
            && let Some(cmd) = config.get(phase).filter(|v| !v.is_null())
        {
            entries.push(cella_features::LifecycleEntry {
                origin: "devcontainer.json".into(),
                command: cmd.clone(),
            });
        }
        if let Some(f) = post_resolve {
            f(&mut entries);
        }
        run_lifecycle_entries(lc_ctx, phase, &entries, progress).await?;
    }

    // Only persist the new content hash when postCreateCommand actually ran.
    // Under prebuild / skip-non-blocking we re-run updateContentCommand alone;
    // writing the hash here would make a later un-gated `up` see a matching
    // hash and permanently skip the deferred postCreateCommand (mirrors the
    // create-path content-hash gating).
    if run_post_create {
        // Compute the hash only on the persist path — the updateContent-only
        // (prebuild / skip-non-blocking) branch never writes it, so re-scanning
        // the workspace there would be wasted work (Copilot review).
        let current_hash = cella_git::content_hash::compute(workspace_root);
        let _ = lc_ctx
            .client
            .exec_command(
                lc_ctx.container_id,
                &ExecOptions {
                    cmd: vec![
                        "sh".to_string(),
                        "-c".to_string(),
                        format!("mkdir -p /tmp/.cella && printf '%s' '{current_hash}' > /tmp/.cella/content_hash"),
                    ],
                    user: lc_ctx.user.map(String::from),
                    env: None,
                    working_dir: None,
                },
            )
            .await;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_string_command() {
        let value = json!("echo hello");
        match parse_lifecycle_command(&value) {
            ParsedLifecycle::Sequential(cmds) => {
                assert_eq!(cmds.len(), 1);
                assert_eq!(cmds[0], vec!["sh", "-c", "echo hello"]);
            }
            ParsedLifecycle::Parallel(_) => panic!("expected Sequential"),
        }
    }

    #[test]
    fn parse_array_command() {
        let value = json!(["echo", "hello"]);
        match parse_lifecycle_command(&value) {
            ParsedLifecycle::Sequential(cmds) => {
                assert_eq!(cmds.len(), 1);
                assert_eq!(cmds[0], vec!["echo", "hello"]);
            }
            ParsedLifecycle::Parallel(_) => panic!("expected Sequential"),
        }
    }

    #[test]
    fn parse_object_commands() {
        let value = json!({"setup": "echo setup", "install": ["npm", "i"]});
        match parse_lifecycle_command(&value) {
            ParsedLifecycle::Parallel(cmds) => {
                assert_eq!(cmds.len(), 2);
            }
            ParsedLifecycle::Sequential(_) => panic!("expected Parallel"),
        }
    }

    #[test]
    fn parse_null_value() {
        let value = json!(null);
        match parse_lifecycle_command(&value) {
            ParsedLifecycle::Sequential(cmds) => {
                assert!(cmds.is_empty());
            }
            ParsedLifecycle::Parallel(_) => panic!("expected Sequential"),
        }
    }

    fn collect_callback_lines(input: &[u8]) -> Vec<String> {
        use std::sync::Mutex;

        let collected = Mutex::new(Vec::new());
        let callback = |line: &str| {
            collected.lock().unwrap().push(line.to_string());
        };

        let masker = SecretMasker::default();
        let mut writer = CallbackWriter::new(&callback, &masker);
        io::Write::write_all(&mut writer, input).unwrap();
        drop(writer);

        collected.into_inner().unwrap()
    }

    #[test]
    fn callback_writer_indents_lines() {
        let lines = collect_callback_lines(b"first line\nsecond line\n");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "      first line");
        assert_eq!(lines[1], "      second line");
    }

    #[test]
    fn callback_writer_masks_secret_values() {
        use std::sync::Mutex;
        let collected = Mutex::new(Vec::new());
        let callback = |line: &str| collected.lock().unwrap().push(line.to_string());
        let masker = SecretMasker::new(&["TOKEN=s3cr3t".to_string()]);
        let mut writer = CallbackWriter::new(&callback, &masker);
        io::Write::write_all(&mut writer, b"echoed s3cr3t value\n").unwrap();
        drop(writer);
        let lines = collected.into_inner().unwrap();
        assert_eq!(lines.len(), 1);
        assert!(!lines[0].contains("s3cr3t"), "secret leaked: {}", lines[0]);
        assert!(
            lines[0].contains("********"),
            "expected mask in {}",
            lines[0]
        );
    }

    #[test]
    fn callback_writer_flushes_partial_line_on_drop() {
        let lines = collect_callback_lines(b"no newline");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0], "      no newline");
    }

    #[test]
    fn callback_writer_skips_blank_lines() {
        let lines = collect_callback_lines(b"content\n\n  \nanother\n");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "      content");
        assert_eq!(lines[1], "      another");
    }

    // ── MaskingWriter tests ──────────────────────────────────────────────────

    // Shared sink for MaskingWriter tests: an `io::Write` backed by an
    // `Arc<Mutex<Vec<u8>>>` so the buffer can be recovered after the writer is
    // dropped.
    use std::sync::{Arc, Mutex};

    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl io::Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Write `input` into a `MaskingWriter` backed by a shared `Vec<u8>` and
    /// return the bytes collected after the writer is dropped (which flushes
    /// any trailing partial line).
    fn masking_writer_bytes(input: &[u8], masker: SecretMasker) -> Vec<u8> {
        let shared = Arc::new(Mutex::new(Vec::new()));
        let mut writer = MaskingWriter::new(SharedBuf(Arc::clone(&shared)), masker);
        io::Write::write_all(&mut writer, input).unwrap();
        drop(writer);
        Arc::try_unwrap(shared).unwrap().into_inner().unwrap()
    }

    #[test]
    fn masking_writer_masks_on_newline() {
        let masker = SecretMasker::new(&["TOKEN=s3cr3t".to_string()]);
        let out = masking_writer_bytes(b"got s3cr3t value\n", masker);
        let s = String::from_utf8(out).unwrap();
        assert!(!s.contains("s3cr3t"), "secret leaked: {s}");
        assert!(s.contains("********"), "mask missing: {s}");
        assert!(s.ends_with('\n'), "newline stripped");
    }

    #[test]
    fn masking_writer_masks_trailing_partial_line_on_drop() {
        // No trailing newline — the remainder is flushed in Drop.
        let masker = SecretMasker::new(&["TOKEN=s3cr3t".to_string()]);
        let out = masking_writer_bytes(b"s3cr3t", masker);
        let s = String::from_utf8(out).unwrap();
        assert!(!s.contains("s3cr3t"), "secret leaked: {s}");
        assert!(s.contains("********"), "mask missing: {s}");
    }

    #[test]
    fn masking_writer_multiple_lines_all_masked() {
        let masker = SecretMasker::new(&["TOKEN=s3cr3t".to_string()]);
        let out = masking_writer_bytes(b"line1 s3cr3t\nline2 s3cr3t\n", masker);
        let s = String::from_utf8(out).unwrap();
        assert!(!s.contains("s3cr3t"), "secret leaked: {s}");
        assert_eq!(s.matches("********").count(), 2);
    }

    #[test]
    fn masking_writer_passthrough_when_no_secrets() {
        let masker = SecretMasker::default();
        let out = masking_writer_bytes(b"hello world\n", masker);
        assert_eq!(out, b"hello world\n");
    }

    #[test]
    fn masking_writer_split_write_masks_correctly() {
        // Secret split across two writes — the buffer holds incomplete bytes
        // until the newline arrives, then masks the full line.
        let masker = SecretMasker::new(&["TOKEN=s3cr3t".to_string()]);
        let shared = Arc::new(Mutex::new(Vec::new()));
        let mut writer = MaskingWriter::new(SharedBuf(Arc::clone(&shared)), masker);
        io::Write::write_all(&mut writer, b"s3cr").unwrap();
        io::Write::write_all(&mut writer, b"3t\n").unwrap();
        drop(writer);
        let out = Arc::try_unwrap(shared).unwrap().into_inner().unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(!s.contains("s3cr3t"), "secret leaked: {s}");
        assert!(s.contains("********"), "mask missing: {s}");
    }

    #[test]
    fn spec_string_command_wrapped_in_sh() {
        let value = json!("echo hello && echo world");
        match parse_lifecycle_command(&value) {
            ParsedLifecycle::Sequential(cmds) => {
                assert_eq!(cmds.len(), 1);
                assert_eq!(cmds[0][0], "sh");
                assert_eq!(cmds[0][1], "-c");
                assert_eq!(cmds[0][2], "echo hello && echo world");
            }
            ParsedLifecycle::Parallel(_) => panic!("expected Sequential"),
        }
    }

    #[test]
    fn spec_array_command_executed_directly() {
        let value = json!(["echo", "hello", "world"]);
        match parse_lifecycle_command(&value) {
            ParsedLifecycle::Sequential(cmds) => {
                assert_eq!(cmds.len(), 1);
                assert_eq!(cmds[0], vec!["echo", "hello", "world"]);
            }
            ParsedLifecycle::Parallel(_) => panic!("expected Sequential"),
        }
    }

    #[test]
    fn spec_object_command_is_parallel() {
        let value = json!({"setup": "npm install", "db": ["mysql", "-u", "root"]});
        match parse_lifecycle_command(&value) {
            ParsedLifecycle::Parallel(cmds) => {
                assert_eq!(cmds.len(), 2);
                let setup = cmds.iter().find(|(n, _)| n == "setup").unwrap();
                assert_eq!(setup.1, vec!["sh", "-c", "npm install"]);
                let db = cmds.iter().find(|(n, _)| n == "db").unwrap();
                assert_eq!(db.1, vec!["mysql", "-u", "root"]);
            }
            ParsedLifecycle::Sequential(_) => panic!("expected Parallel"),
        }
    }

    #[test]
    fn spec_object_string_values_wrapped_in_sh() {
        let value = json!({"server": "npm start", "client": "npm run dev"});
        match parse_lifecycle_command(&value) {
            ParsedLifecycle::Parallel(cmds) => {
                for (_, cmd) in &cmds {
                    assert_eq!(cmd[0], "sh");
                    assert_eq!(cmd[1], "-c");
                }
            }
            ParsedLifecycle::Sequential(_) => panic!("expected Parallel"),
        }
    }

    #[test]
    fn spec_empty_string_is_valid_command() {
        let value = json!("");
        match parse_lifecycle_command(&value) {
            ParsedLifecycle::Sequential(cmds) => {
                assert_eq!(cmds.len(), 1);
                assert_eq!(cmds[0], vec!["sh", "-c", ""]);
            }
            ParsedLifecycle::Parallel(_) => panic!("expected Sequential"),
        }
    }

    #[test]
    fn spec_empty_object_no_parallel_commands() {
        let value = json!({});
        match parse_lifecycle_command(&value) {
            ParsedLifecycle::Parallel(cmds) => assert!(cmds.is_empty()),
            ParsedLifecycle::Sequential(_) => panic!("expected Parallel"),
        }
    }

    #[test]
    fn spec_lifecycle_phase_order() {
        let phases = [
            "initializeCommand",
            "onCreateCommand",
            "updateContentCommand",
            "postCreateCommand",
            "postStartCommand",
            "postAttachCommand",
        ];
        for i in 0..phases.len() - 1 {
            assert!(
                phases.iter().position(|p| *p == phases[i]).unwrap()
                    < phases.iter().position(|p| *p == phases[i + 1]).unwrap()
            );
        }
    }

    #[test]
    fn spec_resume_only_post_start_and_attach() {
        let resume_phases = ["postStartCommand", "postAttachCommand"];
        let creation_only = [
            "initializeCommand",
            "onCreateCommand",
            "updateContentCommand",
            "postCreateCommand",
        ];
        for phase in &creation_only {
            assert!(!resume_phases.contains(phase));
        }
    }

    #[test]
    fn check_exit_code_zero_is_ok() {
        let result = ExecResult {
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
        };
        assert!(
            check_exit_code(&result, "postCreateCommand", None, &SecretMasker::default()).is_ok()
        );
    }

    #[test]
    fn check_exit_code_nonzero_returns_error() {
        let result = ExecResult {
            exit_code: 1,
            stdout: String::new(),
            stderr: "command not found".to_string(),
        };
        let err = check_exit_code(&result, "onCreateCommand", None, &SecretMasker::default())
            .unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("onCreateCommand"),
            "error should contain phase name, got: {msg}"
        );
    }

    #[test]
    fn check_exit_code_nonzero_includes_exit_code_in_message() {
        let result = ExecResult {
            exit_code: 127,
            stdout: String::new(),
            stderr: "sh: npm: not found".to_string(),
        };
        let err = check_exit_code(&result, "postCreateCommand", None, &SecretMasker::default())
            .unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("127"),
            "error should contain exit code, got: {msg}"
        );
    }

    #[test]
    fn check_exit_code_nonzero_includes_stderr() {
        let result = ExecResult {
            exit_code: 2,
            stdout: "some output\n".to_string(),
            stderr: "fatal error occurred\n".to_string(),
        };
        let err = check_exit_code(
            &result,
            "updateContentCommand",
            None,
            &SecretMasker::default(),
        )
        .unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("fatal error occurred"),
            "error should contain trimmed stderr, got: {msg}"
        );
    }

    #[test]
    fn check_exit_code_with_named_prefix() {
        let result = ExecResult {
            exit_code: 1,
            stdout: String::new(),
            stderr: "failed".to_string(),
        };
        let err = check_exit_code(
            &result,
            "postStartCommand",
            Some("setup"),
            &SecretMasker::default(),
        )
        .unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("[setup]"),
            "error should contain [name] prefix, got: {msg}"
        );
    }

    #[test]
    fn check_exit_code_named_prefix_absent_when_none() {
        let result = ExecResult {
            exit_code: 1,
            stdout: String::new(),
            stderr: "err".to_string(),
        };
        let err = check_exit_code(&result, "phase", None, &SecretMasker::default()).unwrap_err();
        let msg = format!("{err}");
        assert!(
            !msg.contains('['),
            "no bracket prefix expected without name, got: {msg}"
        );
    }

    #[test]
    fn check_exit_code_zero_with_name_still_ok() {
        let result = ExecResult {
            exit_code: 0,
            stdout: "done".to_string(),
            stderr: String::new(),
        };
        assert!(check_exit_code(&result, "phase", Some("task"), &SecretMasker::default()).is_ok());
    }

    #[test]
    fn check_exit_code_stderr_trimmed() {
        let result = ExecResult {
            exit_code: 1,
            stdout: String::new(),
            stderr: "  whitespace  \n".to_string(),
        };
        let err = check_exit_code(&result, "phase", None, &SecretMasker::default()).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("whitespace"),
            "expected trimmed stderr in message, got: {msg}"
        );
        assert!(!msg.ends_with('\n'), "stderr should be trimmed, got: {msg}");
    }

    #[test]
    fn check_exit_code_masks_secret_in_stderr() {
        let result = ExecResult {
            exit_code: 1,
            stdout: String::new(),
            stderr: "failed: leaked s3cr3t".to_string(),
        };
        let masker = SecretMasker::new(&["TOKEN=s3cr3t".to_string()]);
        let err = check_exit_code(&result, "phase", None, &masker).unwrap_err();
        let msg = format!("{err}");
        assert!(!msg.contains("s3cr3t"), "secret leaked into error: {msg}");
        assert!(msg.contains("********"), "expected mask, got: {msg}");
    }

    #[test]
    fn parse_object_with_non_string_non_array_value() {
        let value = json!({"check": 42});
        match parse_lifecycle_command(&value) {
            ParsedLifecycle::Parallel(cmds) => {
                assert_eq!(cmds.len(), 1);
                assert_eq!(cmds[0].1[0], "sh");
                assert_eq!(cmds[0].1[1], "-c");
                assert_eq!(cmds[0].1[2], "42");
            }
            ParsedLifecycle::Sequential(_) => panic!("expected Parallel"),
        }
    }

    #[test]
    fn parse_boolean_value() {
        let value = json!(true);
        match parse_lifecycle_command(&value) {
            ParsedLifecycle::Sequential(cmds) => assert!(cmds.is_empty()),
            ParsedLifecycle::Parallel(_) => panic!("expected Sequential"),
        }
    }

    #[test]
    fn parse_number_value() {
        let value = json!(42);
        match parse_lifecycle_command(&value) {
            ParsedLifecycle::Sequential(cmds) => assert!(cmds.is_empty()),
            ParsedLifecycle::Parallel(_) => panic!("expected Sequential"),
        }
    }

    #[test]
    fn parse_array_filters_non_string_elements() {
        let value = json!(["echo", 42, "hello", null]);
        match parse_lifecycle_command(&value) {
            ParsedLifecycle::Sequential(cmds) => {
                assert_eq!(cmds.len(), 1);
                assert_eq!(cmds[0], vec!["echo", "hello"]);
            }
            ParsedLifecycle::Parallel(_) => panic!("expected Sequential"),
        }
    }

    #[test]
    fn parse_empty_array() {
        let value = json!([]);
        match parse_lifecycle_command(&value) {
            ParsedLifecycle::Sequential(cmds) => {
                assert_eq!(cmds.len(), 1);
                assert!(cmds[0].is_empty());
            }
            ParsedLifecycle::Parallel(_) => panic!("expected Sequential"),
        }
    }

    #[test]
    fn callback_writer_handles_multiple_writes_for_one_line() {
        use std::sync::Mutex;

        let collected = Mutex::new(Vec::new());
        let callback = |line: &str| {
            collected.lock().unwrap().push(line.to_string());
        };

        let masker = SecretMasker::default();
        let mut writer = CallbackWriter::new(&callback, &masker);
        io::Write::write_all(&mut writer, b"hello ").unwrap();
        io::Write::write_all(&mut writer, b"world\n").unwrap();
        drop(writer);

        let lines = collected.into_inner().unwrap();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0], "      hello world");
    }

    #[test]
    fn callback_writer_handles_empty_input() {
        let lines = collect_callback_lines(b"");
        assert!(lines.is_empty());
    }

    #[test]
    fn callback_writer_only_newlines() {
        let lines = collect_callback_lines(b"\n\n\n");
        assert!(lines.is_empty());
    }

    // ── Lifecycle phase management tests (from orchestrator) ─────────────

    fn make_resolved_features(
        lifecycle: cella_features::FeatureLifecycle,
    ) -> cella_features::ResolvedFeatures {
        cella_features::ResolvedFeatures {
            features: vec![],
            dockerfile: String::new(),
            build_context: std::path::PathBuf::from("/tmp"),
            container_config: cella_features::FeatureContainerConfig {
                lifecycle,
                ..Default::default()
            },
            metadata_label: String::new(),
            lockfile: None,
        }
    }

    #[test]
    fn resolve_phase_entries_on_create() {
        let mut lifecycle = cella_features::FeatureLifecycle::default();
        lifecycle.on_create.push(cella_features::LifecycleEntry {
            origin: "test-feature".into(),
            command: json!("echo hello"),
        });
        let rf = make_resolved_features(lifecycle);
        let entries = resolve_phase_entries(Some(&rf), "onCreateCommand");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].origin, "test-feature");
        assert_eq!(entries[0].command, json!("echo hello"));
    }

    #[test]
    fn resolve_phase_entries_all_phases() {
        let mut lifecycle = cella_features::FeatureLifecycle::default();
        lifecycle.on_create.push(cella_features::LifecycleEntry {
            origin: "a".into(),
            command: json!("1"),
        });
        lifecycle
            .update_content
            .push(cella_features::LifecycleEntry {
                origin: "b".into(),
                command: json!("2"),
            });
        lifecycle.post_create.push(cella_features::LifecycleEntry {
            origin: "c".into(),
            command: json!("3"),
        });
        lifecycle.post_start.push(cella_features::LifecycleEntry {
            origin: "d".into(),
            command: json!("4"),
        });
        lifecycle.post_attach.push(cella_features::LifecycleEntry {
            origin: "e".into(),
            command: json!("5"),
        });
        let rf = make_resolved_features(lifecycle);
        assert_eq!(resolve_phase_entries(Some(&rf), "onCreateCommand").len(), 1);
        assert_eq!(
            resolve_phase_entries(Some(&rf), "updateContentCommand").len(),
            1
        );
        assert_eq!(
            resolve_phase_entries(Some(&rf), "postCreateCommand").len(),
            1
        );
        assert_eq!(
            resolve_phase_entries(Some(&rf), "postStartCommand").len(),
            1
        );
        assert_eq!(
            resolve_phase_entries(Some(&rf), "postAttachCommand").len(),
            1
        );
    }

    #[test]
    fn resolve_phase_entries_unknown_phase_returns_empty() {
        let rf = make_resolved_features(cella_features::FeatureLifecycle::default());
        let entries = resolve_phase_entries(Some(&rf), "nonExistentPhase");
        assert!(entries.is_empty());
    }

    #[test]
    fn resolve_phase_entries_none_features_returns_empty() {
        let entries = resolve_phase_entries(None, "onCreateCommand");
        assert!(entries.is_empty());
    }

    #[test]
    fn lifecycle_entries_for_phase_from_config_string() {
        let config = json!({"postCreateCommand": "npm install"});
        let entries = lifecycle_entries_for_phase(None, &config, "postCreateCommand");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].origin, "devcontainer.json");
        assert_eq!(entries[0].command, json!("npm install"));
    }

    #[test]
    fn lifecycle_entries_for_phase_from_config_null() {
        let config = json!({"postCreateCommand": null});
        let entries = lifecycle_entries_for_phase(None, &config, "postCreateCommand");
        assert!(entries.is_empty());
    }

    #[test]
    fn lifecycle_entries_for_phase_missing_key() {
        let config = json!({});
        let entries = lifecycle_entries_for_phase(None, &config, "postCreateCommand");
        assert!(entries.is_empty());
    }

    #[test]
    fn lifecycle_entries_wait_for_phase_values() {
        assert_eq!(
            WaitForPhase::from_config(&json!({"waitFor": "initializeCommand"})),
            WaitForPhase::Initialize
        );
        assert_eq!(
            WaitForPhase::from_config(&json!({"waitFor": "onCreateCommand"})),
            WaitForPhase::OnCreate
        );
        assert_eq!(
            WaitForPhase::from_config(&json!({"waitFor": "postCreateCommand"})),
            WaitForPhase::PostCreate
        );
        assert_eq!(
            WaitForPhase::from_config(&json!({"waitFor": "postStartCommand"})),
            WaitForPhase::PostStart
        );
        assert_eq!(
            WaitForPhase::from_config(&json!({})),
            WaitForPhase::UpdateContent
        );
        assert_eq!(
            WaitForPhase::from_config(&json!({"waitFor": "updateContentCommand"})),
            WaitForPhase::UpdateContent
        );
    }

    #[test]
    fn effective_metadata_unchanged_in_branch_a() {
        // hasIdLabels == true: baked metadata drives lifecycle alone; the fresh
        // --config is NOT appended (would double-run hooks).
        let meta = r#"[{"id":"feat","postCreateCommand":"a"}]"#;
        let config = json!({"postCreateCommand": "b", "waitFor": "onCreateCommand"});
        assert_eq!(
            effective_lifecycle_metadata(Some(meta), &config, false).as_deref(),
            Some(meta)
        );
    }

    #[test]
    fn effective_metadata_appends_config_in_branch_b() {
        // hasIdLabels == false (--container-id): on-disk --config appended as
        // the final array entry, carrying only lifecycle + waitFor (NOT
        // remoteEnv, which the caller layers separately).
        let meta = r#"[{"id":"feat","postCreateCommand":"a"}]"#;
        let config = json!({
            "postCreateCommand": "b",
            "waitFor": "onCreateCommand",
            "remoteEnv": {"X": "1"}
        });
        let eff = effective_lifecycle_metadata(Some(meta), &config, true).unwrap();
        let arr: Vec<Value> = serde_json::from_str(&eff).unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[1].get("postCreateCommand").unwrap(), "b");
        assert_eq!(arr[1].get("waitFor").unwrap(), "onCreateCommand");
        assert!(arr[1].get("remoteEnv").is_none());
    }

    #[test]
    fn effective_metadata_none_without_label() {
        let config = json!({"postCreateCommand": "b"});
        assert!(effective_lifecycle_metadata(None, &config, true).is_none());
        assert!(effective_lifecycle_metadata(None, &config, false).is_none());
    }

    #[test]
    fn effective_metadata_branch_b_empty_config_is_noop() {
        let meta = r#"[{"id":"feat","postCreateCommand":"a"}]"#;
        assert_eq!(
            effective_lifecycle_metadata(Some(meta), &json!({}), true).as_deref(),
            Some(meta)
        );
    }

    #[test]
    fn wait_for_metadata_last_entry_wins() {
        // mergeConfiguration: reversed.find(entry => entry.waitFor). Fresh
        // config's waitFor is ignored when a metadata array is present (branch A).
        let meta = r#"[{"waitFor":"onCreateCommand"},{"id":"f","waitFor":"postStartCommand"}]"#;
        assert_eq!(
            WaitForPhase::from_metadata_or_config(
                Some(meta),
                &json!({"waitFor": "initializeCommand"})
            ),
            WaitForPhase::PostStart
        );
    }

    #[test]
    fn wait_for_metadata_absent_defaults_not_config() {
        // Metadata present but no entry declares waitFor → default
        // updateContentCommand, NOT the fresh config's waitFor.
        let meta = r#"[{"id":"f","postCreateCommand":"a"}]"#;
        assert_eq!(
            WaitForPhase::from_metadata_or_config(
                Some(meta),
                &json!({"waitFor": "initializeCommand"})
            ),
            WaitForPhase::UpdateContent
        );
    }

    #[test]
    fn wait_for_falls_back_to_config_without_metadata() {
        // No metadata array (no-label container) → source from config.
        assert_eq!(
            WaitForPhase::from_metadata_or_config(None, &json!({"waitFor": "postCreateCommand"})),
            WaitForPhase::PostCreate
        );
    }

    #[test]
    fn lifecycle_entries_for_phase_from_config_array() {
        let config = json!({"postCreateCommand": ["npm", "install"]});
        let entries = lifecycle_entries_for_phase(None, &config, "postCreateCommand");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].command, json!(["npm", "install"]));
    }

    #[test]
    fn lifecycle_entries_for_phase_from_config_object() {
        let config = json!({
            "postCreateCommand": {
                "build": "npm run build",
                "test": "npm test"
            }
        });
        let entries = lifecycle_entries_for_phase(None, &config, "postCreateCommand");
        assert_eq!(entries.len(), 1);
        assert!(entries[0].command.is_object());
    }

    #[test]
    fn lifecycle_entries_for_phase_from_metadata_label() {
        let metadata = r#"[{"id":"ghcr.io/features/node","postCreateCommand":"npm i"}]"#;
        let entries = lifecycle_entries_for_phase(Some(metadata), &json!({}), "postCreateCommand");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].origin, "ghcr.io/features/node");
        assert_eq!(entries[0].command, json!("npm i"));
    }

    #[test]
    fn lifecycle_entries_for_phase_metadata_missing_phase() {
        let metadata = r#"[{"id":"feature-a","onCreateCommand":"echo hi"}]"#;
        let entries = lifecycle_entries_for_phase(Some(metadata), &json!({}), "postCreateCommand");
        assert!(entries.is_empty());
    }

    #[test]
    fn lifecycle_entries_for_phase_metadata_null_command_skipped() {
        let metadata = r#"[{"id":"feature-a","postCreateCommand":null}]"#;
        let entries = lifecycle_entries_for_phase(Some(metadata), &json!({}), "postCreateCommand");
        assert!(entries.is_empty());
    }

    #[test]
    fn lifecycle_entries_for_phase_metadata_multiple_entries() {
        let metadata = r#"[
            {"id":"feat-a","postStartCommand":"echo a"},
            {"id":"feat-b","postStartCommand":"echo b"}
        ]"#;
        let entries = lifecycle_entries_for_phase(Some(metadata), &json!({}), "postStartCommand");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].origin, "feat-a");
        assert_eq!(entries[1].origin, "feat-b");
    }

    #[test]
    fn lifecycle_entries_for_phase_metadata_no_id_defaults_to_devcontainer_json() {
        let metadata = r#"[{"postCreateCommand":"echo hi"}]"#;
        let entries = lifecycle_entries_for_phase(Some(metadata), &json!({}), "postCreateCommand");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].origin, "devcontainer.json");
    }

    #[test]
    fn lifecycle_entries_for_phase_invalid_metadata_returns_empty() {
        let metadata = "not valid json";
        let entries = lifecycle_entries_for_phase(Some(metadata), &json!({}), "postCreateCommand");
        assert!(entries.is_empty());
    }

    #[test]
    fn resolve_phase_entries_multiple_entries_per_phase() {
        let mut lifecycle = cella_features::FeatureLifecycle::default();
        lifecycle.on_create.push(cella_features::LifecycleEntry {
            origin: "feat-a".into(),
            command: json!("echo a"),
        });
        lifecycle.on_create.push(cella_features::LifecycleEntry {
            origin: "feat-b".into(),
            command: json!(["npm", "install"]),
        });
        let rf = make_resolved_features(lifecycle);
        let entries = resolve_phase_entries(Some(&rf), "onCreateCommand");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].command, json!("echo a"));
        assert_eq!(entries[1].command, json!(["npm", "install"]));
    }

    #[test]
    fn resolve_phase_entries_empty_lifecycle() {
        let rf = make_resolved_features(cella_features::FeatureLifecycle::default());
        for phase in &[
            "onCreateCommand",
            "updateContentCommand",
            "postCreateCommand",
            "postStartCommand",
            "postAttachCommand",
        ] {
            assert!(resolve_phase_entries(Some(&rf), phase).is_empty());
        }
    }

    #[test]
    fn wait_for_phase_ordinal_ordering() {
        assert!(WaitForPhase::Initialize.ordinal() < WaitForPhase::OnCreate.ordinal());
        assert!(WaitForPhase::OnCreate.ordinal() < WaitForPhase::UpdateContent.ordinal());
        assert!(WaitForPhase::UpdateContent.ordinal() < WaitForPhase::PostCreate.ordinal());
        assert!(WaitForPhase::PostCreate.ordinal() < WaitForPhase::PostStart.ordinal());
    }

    #[test]
    fn wait_for_phase_unknown_string_defaults_to_update_content() {
        assert_eq!(
            WaitForPhase::from_config(&json!({"waitFor": "somethingBogus"})),
            WaitForPhase::UpdateContent
        );
    }

    #[test]
    fn wait_for_phase_null_value_defaults_to_update_content() {
        assert_eq!(
            WaitForPhase::from_config(&json!({"waitFor": null})),
            WaitForPhase::UpdateContent
        );
    }

    /// Map a phase plan to the set of phase names per action, for readable
    /// assertions in the table test.
    fn plan_sets(plan: [PhaseAction; 5]) -> (Vec<&'static str>, Vec<&'static str>) {
        let mut foreground = Vec::new();
        let mut background = Vec::new();
        for (i, action) in plan.iter().enumerate() {
            match action {
                PhaseAction::Foreground => foreground.push(POST_CREATE_PHASES[i]),
                PhaseAction::Background => background.push(POST_CREATE_PHASES[i]),
                PhaseAction::Skip => {}
            }
        }
        (foreground, background)
    }

    /// Test helper: gate for `--skip-post-create`.
    fn gate_skip_post_create() -> LifecycleGate {
        LifecycleGate::new(
            WaitForPhase::UpdateContent,
            true,
            StopAfter::default(),
            false,
        )
    }

    /// Test helper: gate for `--skip-non-blocking-commands` at a given waitFor.
    fn gate_skip_non_blocking(wait_for: WaitForPhase) -> LifecycleGate {
        LifecycleGate::new(
            wait_for,
            false,
            StopAfter {
                skip_non_blocking: true,
                prebuild: false,
            },
            false,
        )
    }

    /// Test helper: gate for `--prebuild` at a given waitFor.
    fn gate_prebuild(wait_for: WaitForPhase) -> LifecycleGate {
        LifecycleGate::new(
            wait_for,
            false,
            StopAfter {
                skip_non_blocking: false,
                prebuild: true,
            },
            false,
        )
    }

    /// Test helper: gate for `--skip-post-attach`.
    fn gate_skip_post_attach() -> LifecycleGate {
        LifecycleGate::new(
            WaitForPhase::UpdateContent,
            false,
            StopAfter::default(),
            true,
        )
    }

    #[test]
    fn lifecycle_gate_default_matches_legacy_behavior() {
        // Default: foreground onCreate+updateContent, background the rest.
        let gate = LifecycleGate::default();
        assert!(gate.enabled);
        assert_eq!(gate.wait_for, WaitForPhase::UpdateContent);
        assert!(gate.background_tail());
        assert!(!gate.rerun_update_content());
        assert!(!gate.skip_post_attach);
        let (fg, bg) = plan_sets(plan_phases(gate));
        assert_eq!(fg, vec!["onCreateCommand", "updateContentCommand"]);
        assert_eq!(
            bg,
            vec!["postCreateCommand", "postStartCommand", "postAttachCommand"]
        );
    }

    #[test]
    fn plan_phases_table_driven() {
        // Each row: (gate, expected_foreground, expected_background).
        let none: Vec<&str> = vec![];
        let cases: &[(LifecycleGate, Vec<&str>, Vec<&str>)] = &[
            // --skip-post-create: nothing runs at all.
            (gate_skip_post_create(), none.clone(), none.clone()),
            // default (no flags): foreground to updateContent, background the tail.
            (
                LifecycleGate::default(),
                vec!["onCreateCommand", "updateContentCommand"],
                vec!["postCreateCommand", "postStartCommand", "postAttachCommand"],
            ),
            // --skip-non-blocking-commands (default waitFor): same fg, tail DROPPED.
            (
                gate_skip_non_blocking(WaitForPhase::UpdateContent),
                vec!["onCreateCommand", "updateContentCommand"],
                none.clone(),
            ),
            // --skip-non-blocking-commands with waitFor=onCreateCommand: only onCreate fg.
            (
                gate_skip_non_blocking(WaitForPhase::OnCreate),
                vec!["onCreateCommand"],
                none.clone(),
            ),
            // --prebuild: stop after onCreate+updateContent, tail dropped.
            (
                gate_prebuild(WaitForPhase::UpdateContent),
                vec!["onCreateCommand", "updateContentCommand"],
                none.clone(),
            ),
            // --prebuild overrides config waitFor=postCreateCommand (still stops after updateContent).
            (
                gate_prebuild(WaitForPhase::PostCreate),
                vec!["onCreateCommand", "updateContentCommand"],
                none.clone(),
            ),
            // --skip-post-attach: default set, but postAttach dropped from the tail.
            (
                gate_skip_post_attach(),
                vec!["onCreateCommand", "updateContentCommand"],
                vec!["postCreateCommand", "postStartCommand"],
            ),
        ];

        for (i, (gate, want_fg, want_bg)) in cases.iter().enumerate() {
            let (fg, bg) = plan_sets(plan_phases(*gate));
            assert_eq!(&fg, want_fg, "foreground mismatch in case {i}: {gate:?}");
            assert_eq!(&bg, want_bg, "background mismatch in case {i}: {gate:?}");
        }
    }

    #[test]
    fn lifecycle_gate_prebuild_overrides_wait_for_and_forces_rerun() {
        let gate = gate_prebuild(WaitForPhase::PostStart);
        assert!(gate.stop.prebuild);
        assert!(gate.rerun_update_content());
        assert!(!gate.background_tail());
        // waitFor=postStart is ignored by prebuild: boundary is after updateContent.
        assert_eq!(gate.foreground_boundary(), PHASE_UPDATE_CONTENT + 1);
    }

    #[test]
    fn lifecycle_gate_skip_non_blocking_precedence_over_prebuild() {
        // Spec (prebuild edge case 4 + verification): `--skip-non-blocking
        // --prebuild` with waitFor=onCreateCommand stops AFTER onCreate —
        // updateContent never runs (skipNonBlocking's earlier stop wins).
        let gate = LifecycleGate::new(
            WaitForPhase::OnCreate,
            false,
            StopAfter {
                skip_non_blocking: true,
                prebuild: true,
            },
            false,
        );
        assert!(!gate.background_tail());
        let (fg, bg) = plan_sets(plan_phases(gate));
        assert_eq!(fg, vec!["onCreateCommand"]);
        assert!(bg.is_empty());
    }

    #[test]
    fn lifecycle_gate_both_flags_default_wait_for_run_same_set() {
        // With default waitFor, both flags stop at the same point — the
        // executed set is identical (onCreate+updateContent).
        let gate = LifecycleGate::new(
            WaitForPhase::UpdateContent,
            false,
            StopAfter {
                skip_non_blocking: true,
                prebuild: true,
            },
            false,
        );
        let (fg, bg) = plan_sets(plan_phases(gate));
        assert_eq!(fg, vec!["onCreateCommand", "updateContentCommand"]);
        assert!(bg.is_empty());
    }

    #[test]
    fn lifecycle_gate_disabled_skips_everything() {
        let gate = LifecycleGate::new(WaitForPhase::PostStart, true, StopAfter::default(), false);
        assert!(!gate.enabled);
        let plan = plan_phases(gate);
        assert!(plan.iter().all(|a| *a == PhaseAction::Skip));
    }

    #[test]
    fn lifecycle_state_default_values() {
        let state = LifecycleState::default();
        assert!(!state.oncreate_done);
    }

    #[test]
    fn lifecycle_state_roundtrip_serde() {
        let state = LifecycleState {
            oncreate_done: true,
            started_at: Some("2026-06-15T12:00:00Z".to_owned()),
        };
        let json = serde_json::to_string(&state).unwrap();
        let deserialized: LifecycleState = serde_json::from_str(&json).unwrap();
        assert!(deserialized.oncreate_done);
    }

    #[test]
    fn lifecycle_state_deserialize_empty_object() {
        let state: LifecycleState = serde_json::from_str("{}").unwrap();
        assert!(!state.oncreate_done);
    }

    #[test]
    fn lifecycle_state_deserialize_partial() {
        let state: LifecycleState = serde_json::from_str(r#"{"oncreate_done": true}"#).unwrap();
        assert!(state.oncreate_done);
    }

    #[test]
    fn hash_lifecycle_entries_deterministic() {
        let entries = vec![cella_features::LifecycleEntry {
            origin: "feature-a".into(),
            command: json!("npm install"),
        }];
        let h1 = hash_lifecycle_entries(&entries);
        let h2 = hash_lifecycle_entries(&entries);
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64);
    }

    #[test]
    fn hash_lifecycle_entries_different_for_different_entries() {
        let entries_a = vec![cella_features::LifecycleEntry {
            origin: "a".into(),
            command: json!("echo a"),
        }];
        let entries_b = vec![cella_features::LifecycleEntry {
            origin: "b".into(),
            command: json!("echo b"),
        }];
        assert_ne!(
            hash_lifecycle_entries(&entries_a),
            hash_lifecycle_entries(&entries_b)
        );
    }

    #[test]
    fn hash_lifecycle_entries_empty() {
        let h = hash_lifecycle_entries(&[]);
        assert_eq!(h.len(), 64);
    }

    #[test]
    fn resolve_entries_tier1_features_take_priority() {
        let mut lifecycle = cella_features::FeatureLifecycle::default();
        lifecycle.on_create.push(cella_features::LifecycleEntry {
            origin: "feature-x".into(),
            command: json!("from features"),
        });
        let rf = make_resolved_features(lifecycle);
        let metadata = r#"[{"id":"img","onCreateCommand":"from metadata"}]"#;
        let config = json!({"onCreateCommand": "from config"});

        let entries =
            resolve_entries_with_metadata(Some(&rf), Some(metadata), &config, "onCreateCommand");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].origin, "feature-x");
    }

    #[test]
    fn resolve_entries_tier2_metadata_when_no_features() {
        let metadata = r#"[{"id":"prebuilt","onCreateCommand":"from metadata"}]"#;
        let config = json!({"onCreateCommand": "from config"});

        let entries =
            resolve_entries_with_metadata(None, Some(metadata), &config, "onCreateCommand");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].origin, "prebuilt");
        assert_eq!(entries[0].command, json!("from metadata"));
    }

    #[test]
    fn resolve_entries_tier3_config_when_no_features_or_metadata() {
        let config = json!({"onCreateCommand": "from config"});

        let entries = resolve_entries_with_metadata(None, None, &config, "onCreateCommand");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].origin, "devcontainer.json");
        assert_eq!(entries[0].command, json!("from config"));
    }

    #[test]
    fn resolve_entries_tier3_config_when_metadata_lacks_phase() {
        let metadata = r#"[{"id":"prebuilt","postStartCommand":"echo hi"}]"#;
        let config = json!({"onCreateCommand": "from config"});

        let entries =
            resolve_entries_with_metadata(None, Some(metadata), &config, "onCreateCommand");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].origin, "devcontainer.json");
    }

    #[test]
    fn resolve_entries_empty_when_nothing_defined() {
        let config = json!({});
        let entries = resolve_entries_with_metadata(None, None, &config, "onCreateCommand");
        assert!(entries.is_empty());
    }

    #[test]
    fn resolve_entries_empty_features_falls_through_to_metadata() {
        let rf = make_resolved_features(cella_features::FeatureLifecycle::default());
        let metadata = r#"[{"id":"prebuilt","postCreateCommand":"setup"}]"#;
        let config = json!({});

        let entries =
            resolve_entries_with_metadata(Some(&rf), Some(metadata), &config, "postCreateCommand");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].origin, "prebuilt");
    }

    #[test]
    fn entry_to_shell_command_string() {
        let entry = cella_features::LifecycleEntry {
            origin: "test".into(),
            command: json!("npm install"),
        };
        assert_eq!(entry_to_shell_command(&entry), "npm install");
    }

    #[test]
    fn entry_to_shell_command_array() {
        let entry = cella_features::LifecycleEntry {
            origin: "test".into(),
            command: json!(["echo", "hello world"]),
        };
        assert_eq!(entry_to_shell_command(&entry), "echo 'hello world'");
    }

    #[test]
    fn entry_to_shell_command_object_tracks_pids() {
        let entry = cella_features::LifecycleEntry {
            origin: "test".into(),
            command: json!({"build": "npm run build", "lint": "npm run lint"}),
        };
        let cmd = entry_to_shell_command(&entry);
        assert!(cmd.contains("_cella_pids"));
        assert!(cmd.contains("wait"));
        assert!(cmd.contains("exit 1"));
    }

    #[test]
    fn entry_to_shell_command_null_is_noop() {
        let entry = cella_features::LifecycleEntry {
            origin: "test".into(),
            command: json!(null),
        };
        assert_eq!(entry_to_shell_command(&entry), "true");
    }
}
