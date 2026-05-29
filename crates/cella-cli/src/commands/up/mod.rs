use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use clap::Args;
use serde_json::json;
use tracing::{debug, warn};

use super::{
    BuildKitMode, ComposePullPolicy, GpuAvailability, ImagePullPolicy, LogFormat, LogLevel,
    MountConsistency, OutputFormat, StrictnessLevel, UpdateRemoteUserUidDefault,
};

use cella_backend::{BuildSecret, ContainerBackend, ExecOptions, MountConfig, container_name};
use cella_config::devcontainer::resolve::{self, ResolvedConfig};
use cella_orchestrator::env_cache::probe_and_cache_user_env;
use cella_orchestrator::shell_detect::detect_shell;

/// Build and container-management flags for an `up` invocation.
#[derive(Args)]
pub struct UpBuildArgs {
    /// Rebuild the container image before starting.
    #[arg(long)]
    pub(crate) rebuild: bool,

    /// Do not use cache when building the image.
    #[arg(long)]
    pub(crate) build_no_cache: bool,

    /// Remove existing container before starting.
    #[arg(long)]
    pub(crate) remove_existing_container: bool,

    /// Image pull policy.
    #[arg(long, value_enum)]
    pub(crate) pull: Option<ImagePullPolicy>,

    /// `BuildKit` secret to pass to the build (format: `id=X[,src=Y][,env=Z]`).
    /// Can be specified multiple times.
    #[arg(long = "secret")]
    pub(crate) secrets: Vec<String>,

    /// JSON file mapping secret names to values, injected as env vars into
    /// lifecycle commands only (never stored in labels or image layers).
    #[arg(long = "secrets-file")]
    pub(crate) secrets_file: Option<PathBuf>,

    /// Additional image(s) to use as a layer cache during the build (repeatable).
    #[arg(long = "cache-from")]
    pub(crate) cache_from: Vec<String>,

    /// Cache export destination for the build (`BuildKit` `--cache-to`).
    #[arg(long = "cache-to")]
    pub(crate) cache_to: Option<String>,

    /// Control whether `BuildKit` is used when building images.
    #[arg(long, value_enum, default_value = "auto")]
    pub(crate) buildkit: BuildKitMode,

    /// Path to the Docker CLI binary (used for image builds and compose).
    #[arg(long = "docker-path")]
    pub(crate) docker_path: Option<String>,

    /// Path to the Docker Compose CLI binary.
    #[arg(long = "docker-compose-path")]
    pub(crate) docker_compose_path: Option<String>,
}

/// Mount-related flags for an `up` invocation.
#[derive(Args)]
pub struct UpMountArgs {
    /// Additional mount point(s).
    /// Format: type=<bind|volume>,source=<source>,target=<target>[,external=<true|false>]
    #[arg(long = "mount")]
    pub(crate) mount: Vec<String>,

    /// Workspace mount consistency (ignored on Linux).
    #[arg(long, value_enum, default_value = "cached")]
    pub(crate) workspace_mount_consistency: MountConsistency,

    /// Mount the workspace using its Git root (default: true).
    #[arg(long, action = clap::ArgAction::Set, default_value_t = true)]
    pub(crate) mount_workspace_git_root: bool,

    /// Mount the Git worktree common dir for Git operations in the container.
    /// Requires the worktree to be created with relative paths.
    #[arg(long)]
    pub(crate) mount_git_worktree_common_dir: bool,
}

/// Validate an `--id-label` value (`name=value`, both non-empty).
fn parse_id_label(s: &str) -> Result<String, String> {
    match s.split_once('=') {
        Some((k, v)) if !k.is_empty() && !v.is_empty() => Ok(s.to_string()),
        _ => Err("id-label must match <name>=<value>".to_string()),
    }
}

/// Validate a `--remote-env` value (`name=value`, value may be empty).
fn parse_remote_env(s: &str) -> Result<String, String> {
    match s.split_once('=') {
        Some((k, _)) if !k.is_empty() => Ok(s.to_string()),
        _ => Err("remote-env must match <name>=<value>".to_string()),
    }
}

/// Configuration-input flags: container targeting, config overrides, and the
/// feature/lifecycle inputs that mirror the official `up` surface.
#[derive(Args)]
pub struct UpConfigInputArgs {
    /// Id label(s) of the format `name=value`, used to find/tag the container
    /// (repeatable). If omitted, one is inferred from the workspace folder.
    #[arg(long = "id-label", value_parser = parse_id_label)]
    pub(crate) id_label: Vec<String>,

    /// devcontainer.json path that overrides any discovered config entirely.
    #[arg(long = "override-config")]
    pub(crate) override_config: Option<PathBuf>,

    /// Additional features to apply (JSON, as in the "features" section).
    #[arg(long = "additional-features")]
    pub(crate) additional_features: Option<String>,

    /// Remote environment variables of the format `name=value`, added when
    /// running the user (lifecycle) commands (repeatable).
    #[arg(long = "remote-env", value_parser = parse_remote_env)]
    pub(crate) remote_env: Vec<String>,

    /// Do not run postAttachCommand.
    #[arg(long)]
    pub(crate) skip_post_attach: bool,

    /// Fail if the container does not already exist.
    #[arg(long)]
    pub(crate) expect_existing_container: bool,

    /// Disable automatic feature id mapping (testing only).
    #[arg(long, hide = true)]
    pub(crate) skip_feature_auto_mapping: bool,
}

/// Lifecycle-gating flags for an `up` invocation.
#[derive(Args)]
pub struct UpLifecycleArgs {
    /// Do not run onCreate/updateContent/postCreate/postStart/postAttach
    /// commands and do not install dotfiles.
    #[arg(long)]
    pub(crate) skip_post_create: bool,

    /// Stop running user commands after the `waitFor` phase (default
    /// updateContentCommand).
    #[arg(long)]
    pub(crate) skip_non_blocking_commands: bool,

    /// Stop after onCreateCommand and updateContentCommand.
    #[arg(long)]
    pub(crate) prebuild: bool,
}

/// Result-shaping flags for an `up` invocation's JSON output.
#[derive(Args)]
pub struct UpResultArgs {
    /// Include the configuration in the JSON result.
    #[arg(long)]
    pub(crate) include_configuration: bool,

    /// Include the merged configuration in the JSON result.
    #[arg(long)]
    pub(crate) include_merged_configuration: bool,

    /// Omit remoteEnv from the container metadata label.
    #[arg(long, hide = true)]
    pub(crate) omit_config_remote_env_from_metadata: bool,
}

/// Feature-lockfile flags. cella does not use feature lockfiles; these are
/// accepted for devcontainer-CLI compatibility and are no-ops.
#[derive(Args)]
pub struct UpLockfileArgs {
    /// Disable lockfile generation and verification (compatibility no-op).
    #[arg(
        long,
        conflicts_with_all = ["frozen_lockfile", "experimental_lockfile", "experimental_frozen_lockfile"],
    )]
    pub(crate) no_lockfile: bool,

    /// Ensure the lockfile remains unchanged (compatibility no-op).
    #[arg(long)]
    pub(crate) frozen_lockfile: bool,

    /// Deprecated alias for lockfile writing (compatibility no-op).
    #[arg(long, hide = true)]
    pub(crate) experimental_lockfile: bool,
}

/// Dotfiles flags for an `up` invocation.
#[derive(Args)]
pub struct UpDotfilesArgs {
    /// URL of a dotfiles Git repository to clone into the container.
    #[arg(long = "dotfiles-repository")]
    pub(crate) repository: Option<String>,

    /// Command to run after cloning the dotfiles repository. Defaults to the
    /// first of install.sh, install, bootstrap.sh, bootstrap, setup.sh, setup.
    #[arg(long = "dotfiles-install-command")]
    pub(crate) install_command: Option<String>,

    /// Path to clone the dotfiles repository to (default `~/dotfiles`).
    #[arg(long = "dotfiles-target-path", default_value = "~/dotfiles")]
    pub(crate) target_path: String,
}

/// Compatibility/diagnostic flags accepted for devcontainer-CLI parity.
///
/// The data-folder fields and `omit_syntax_directive`/`experimental_frozen_lockfile`
/// are no-ops in cella (it manages its own data dirs and has no feature
/// lockfiles); the rest are wired into behavior in later phases.
#[derive(Args)]
pub struct UpCompatArgs {
    /// Container data folder for in-container user data (compatibility no-op).
    #[arg(long = "container-data-folder")]
    pub(crate) container_data_folder: Option<PathBuf>,

    /// Container system data folder (compatibility no-op).
    #[arg(long = "container-system-data-folder")]
    pub(crate) container_system_data_folder: Option<PathBuf>,

    /// Per-session cache folder inside the container (compatibility no-op).
    #[arg(long = "container-session-data-folder")]
    pub(crate) container_session_data_folder: Option<PathBuf>,

    /// Host directory persisted across sessions (compatibility no-op).
    #[arg(long = "user-data-folder")]
    pub(crate) user_data_folder: Option<PathBuf>,

    /// Availability of GPUs for dev containers that request one.
    #[arg(long = "gpu-availability", value_enum, default_value = "detect")]
    pub(crate) gpu_availability: GpuAvailability,

    /// Default for updating the remote user's UID/GID to the local user's.
    #[arg(
        long = "update-remote-user-uid-default",
        value_enum,
        default_value = "on"
    )]
    pub(crate) update_remote_user_uid_default: UpdateRemoteUserUidDefault,

    /// Log verbosity for lifecycle/terminal logging.
    #[arg(long = "log-level", value_enum)]
    pub(crate) log_level: Option<LogLevel>,

    /// Log output format.
    #[arg(long = "log-format", value_enum, default_value = "text")]
    pub(crate) log_format: LogFormat,

    // `--terminal-columns` / `--terminal-rows` are a true no-op for `up`. The
    // official CLI feeds these to node-pty when sizing lifecycle subprocesses,
    // but cella runs lifecycle commands through bollard capture exec
    // (`ExecOptions`, which has no PTY/cols/rows) — there is no PTY to size on
    // the `up` path. The only PTY-sizing code in cella is the interactive
    // `exec`/`shell` path (cella-docker `exec_interactive`), which reads the
    // live local terminal via `crossterm::terminal::size()` and is never
    // reached by `up`. Accepted-and-ignored for drop-in parity; clap's
    // `requires` enforces the official both-required pairing. If lifecycle ever
    // moves to inherited-stdio/PTY shell-outs, exporting these as COLUMNS/LINES
    // is the natural future home — do not wire a dead field now.
    /// Number of columns to render subprocess output for.
    #[arg(long = "terminal-columns", requires = "terminal_rows")]
    pub(crate) terminal_columns: Option<u16>,

    /// Number of rows to render subprocess output for.
    #[arg(long = "terminal-rows", requires = "terminal_columns")]
    pub(crate) terminal_rows: Option<u16>,

    /// Ensure the lockfile exists and remains unchanged (compatibility no-op).
    #[arg(long, hide = true)]
    pub(crate) experimental_frozen_lockfile: bool,

    // `--omit-syntax-directive` is a true no-op in cella. The official CLI has
    // two effects: (A) it parses the user's Dockerfile in JS and strips any
    // `# syntax=` directive (a moby/buildkit#4556 workaround), and (B) it
    // suppresses the `# syntax=` line it would otherwise prepend to its
    // generated feature-extension Dockerfile. cella does NEITHER: it never
    // parses Dockerfile content (`BuildOptions.dockerfile` is a filename passed
    // verbatim as `-f`; the docker engine reads any `# syntax=` natively), and
    // its generated feature Dockerfile emits no `# syntax=` line at all. So
    // cella already behaves as if this flag is permanently on — there is
    // nothing to suppress. Accepted-and-ignored for drop-in parity.
    /// Omit Dockerfile syntax directives (compatibility no-op).
    #[arg(long, hide = true)]
    pub(crate) omit_syntax_directive: bool,
}

/// Start a dev container for the current workspace.
#[derive(Args)]
pub struct UpArgs {
    #[command(flatten)]
    pub verbose: super::VerboseArgs,

    #[command(flatten)]
    pub(crate) build: UpBuildArgs,

    /// Explicit workspace folder path (defaults to current directory).
    #[arg(long)]
    pub(crate) workspace_folder: Option<PathBuf>,

    #[command(flatten)]
    pub(crate) backend: crate::backend::BackendArgs,

    /// Path to devcontainer.json (overrides auto-discovery).
    #[arg(long)]
    pub(crate) config: Option<PathBuf>,

    /// Output format. `auto` (default) emits the JSON result envelope when
    /// stdout is piped/scripted and the human text line when attached to a
    /// terminal; `text`/`json` force the respective format.
    #[arg(long, value_enum, default_value = "auto")]
    pub(crate) output: OutputFormat,

    /// Strictness level for validation.
    #[arg(long, value_enum)]
    pub(crate) strict: Vec<StrictnessLevel>,

    /// Skip SHA256 checksum verification for agent binary download.
    #[arg(long)]
    pub(crate) skip_checksum: bool,

    /// Target a worktree branch's container by branch name.
    #[arg(long)]
    pub(crate) branch: Option<String>,

    /// Start container without network blocking rules (proxy forwarding still active).
    #[arg(long)]
    pub(crate) no_network_rules: bool,

    /// Docker Compose profile(s) to activate (repeatable).
    #[arg(long = "profile")]
    pub(crate) profile: Vec<String>,

    /// Extra env-file(s) to pass to Docker Compose (repeatable).
    #[arg(long = "env-file")]
    pub(crate) env_file: Vec<PathBuf>,

    /// Pull policy for Docker Compose services.
    #[arg(long = "pull-policy", value_enum)]
    pub(crate) pull_policy: Option<ComposePullPolicy>,

    #[command(flatten)]
    pub(crate) mounts: UpMountArgs,

    #[command(flatten)]
    pub(crate) config_inputs: UpConfigInputArgs,

    #[command(flatten)]
    pub(crate) lifecycle: UpLifecycleArgs,

    #[command(flatten)]
    pub(crate) result: UpResultArgs,

    #[command(flatten)]
    pub(crate) lockfile: UpLockfileArgs,

    #[command(flatten)]
    pub(crate) dotfiles: UpDotfilesArgs,

    #[command(flatten)]
    pub(crate) compat: UpCompatArgs,

    /// Default value for userEnvProbe when devcontainer.json doesn't specify one.
    #[arg(long, value_enum, default_value_t = cella_env::user_env_probe::UserEnvProbe::LoginInteractiveShell)]
    pub(crate) default_user_env_probe: cella_env::user_env_probe::UserEnvProbe,
}

impl UpArgs {
    /// Whether spinners should run. Spinners are independent of the resolved
    /// stdout result format: they stay on in `Auto`/`Text` mode and are only
    /// disabled by an explicit `--output json`. (main.rs additionally gates
    /// them on a TTY stderr and an unset `RUST_LOG`.)
    pub const fn is_text_output(&self) -> bool {
        matches!(self.output, OutputFormat::Auto | OutputFormat::Text)
    }

    /// Emit a debug trace acknowledging every devcontainer-CLI-parity flag.
    ///
    /// Declaring the full official flag surface is what makes cella a drop-in
    /// replacement (clap rejects unknown args), but the behavioral flags are
    /// wired into the orchestrator in later phases. This trace both documents
    /// accepted-but-not-yet-wired flags and is the complete, correct behavior
    /// for the compatibility no-ops (data folders, lockfiles, syntax directive)
    /// — cella has no equivalent state, so accepting and ignoring them yields
    /// correct results.
    fn acknowledge_compat_flags(&self) {
        let ci = &self.config_inputs;
        let lc = &self.lifecycle;
        debug!(
            id_labels = ci.id_label.len(),
            override_config = ?ci.override_config,
            additional_features = ci.additional_features.is_some(),
            remote_env = ci.remote_env.len(),
            skip_post_create = lc.skip_post_create,
            skip_non_blocking_commands = lc.skip_non_blocking_commands,
            prebuild = lc.prebuild,
            skip_post_attach = ci.skip_post_attach,
            expect_existing_container = ci.expect_existing_container,
            skip_feature_auto_mapping = ci.skip_feature_auto_mapping,
            "up: config/lifecycle flags accepted"
        );

        let b = &self.build;
        let c = &self.compat;
        let r = &self.result;
        debug!(
            cache_from = b.cache_from.len(),
            cache_to = ?b.cache_to,
            buildkit = b.buildkit.as_str(),
            docker_path = ?b.docker_path,
            docker_compose_path = ?b.docker_compose_path,
            gpu_availability = c.gpu_availability.as_str(),
            update_remote_user_uid_default = c.update_remote_user_uid_default.as_str(),
            log_level = ?c.log_level.map(LogLevel::as_str),
            log_format = c.log_format.as_str(),
            terminal_columns = ?c.terminal_columns,
            terminal_rows = ?c.terminal_rows,
            include_configuration = r.include_configuration,
            include_merged_configuration = r.include_merged_configuration,
            omit_config_remote_env_from_metadata = r.omit_config_remote_env_from_metadata,
            "up: build/backend/result flags accepted"
        );

        let lf = &self.lockfile;
        debug!(
            container_data_folder = ?c.container_data_folder,
            container_system_data_folder = ?c.container_system_data_folder,
            container_session_data_folder = ?c.container_session_data_folder,
            user_data_folder = ?c.user_data_folder,
            no_lockfile = lf.no_lockfile,
            frozen_lockfile = lf.frozen_lockfile,
            experimental_lockfile = lf.experimental_lockfile,
            experimental_frozen_lockfile = c.experimental_frozen_lockfile,
            omit_syntax_directive = c.omit_syntax_directive,
            "up: compatibility no-op flags accepted"
        );
    }
}

fn parse_cli_mount(s: &str) -> Result<MountConfig, String> {
    let mut mount_type = None;
    let mut source = None;
    let mut target = None;
    let mut external = false;

    for part in s.split(',') {
        if let Some((key, value)) = part.split_once('=') {
            match key {
                "type" => {
                    if value != "bind" && value != "volume" {
                        return Err(format!(
                            "invalid mount type '{value}': expected 'bind' or 'volume'\n\
                             Expected: type=<bind|volume>,source=<source>,target=<target>[,external=<true|false>]"
                        ));
                    }
                    mount_type = Some(value.to_string());
                }
                "source" | "src" => source = Some(value.to_string()),
                "target" | "dst" | "destination" => target = Some(value.to_string()),
                "external" => match value {
                    "true" => external = true,
                    "false" => {}
                    _ => {
                        return Err(format!(
                            "invalid external value '{value}': expected 'true' or 'false'"
                        ));
                    }
                },
                _ => {
                    return Err(format!(
                        "unknown mount key '{key}'\n\
                         Expected: type=<bind|volume>,source=<source>,target=<target>[,external=<true|false>]"
                    ));
                }
            }
        } else {
            return Err(format!(
                "invalid mount format: {s}\n\
                 Expected: type=<bind|volume>,source=<source>,target=<target>[,external=<true|false>]"
            ));
        }
    }

    let Some(mount_type) = mount_type else {
        return Err(format!(
            "missing 'type' in mount: {s}\n\
             Expected: type=<bind|volume>,source=<source>,target=<target>[,external=<true|false>]"
        ));
    };
    let Some(source) = source else {
        return Err(format!(
            "missing 'source' in mount: {s}\n\
             Expected: type=<bind|volume>,source=<source>,target=<target>[,external=<true|false>]"
        ));
    };
    let Some(target) = target else {
        return Err(format!(
            "missing 'target' in mount: {s}\n\
             Expected: type=<bind|volume>,source=<source>,target=<target>[,external=<true|false>]"
        ));
    };

    Ok(MountConfig {
        mount_type,
        source,
        target,
        consistency: None,
        read_only: false,
        external,
    })
}

fn parse_secrets_file(
    path: &Path,
) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read secrets file '{}': {e}", path.display()))?;
    let parsed: serde_json::Value = serde_json::from_str(&content)
        .map_err(|e| format!("Invalid JSON in secrets file '{}': {e}", path.display()))?;
    let obj = parsed.as_object().ok_or_else(|| {
        format!(
            "Secrets file '{}' must contain a JSON object",
            path.display()
        )
    })?;
    let mut secrets = Vec::with_capacity(obj.len());
    for (key, value) in obj {
        let val = value
            .as_str()
            .ok_or_else(|| format!("Secret '{key}' must be a string value"))?;
        secrets.push(format!("{key}={val}"));
    }
    Ok(secrets)
}

fn compute_default_workspace_folder(workspace_root: &Path, host_mount_folder: &Path) -> String {
    let mount_basename = host_mount_folder.file_name().map_or_else(
        || "workspace".to_string(),
        |n| n.to_string_lossy().to_string(),
    );
    let container_mount_folder = format!("/workspaces/{mount_basename}");
    if host_mount_folder == workspace_root {
        return container_mount_folder;
    }
    if let Ok(rel) = workspace_root.strip_prefix(host_mount_folder) {
        let rel_posix = rel.to_string_lossy().replace('\\', "/");
        if !rel_posix.is_empty() {
            return format!("{container_mount_folder}/{rel_posix}");
        }
    }
    container_mount_folder
}

use cella_orchestrator::NetworkRulePolicy;

/// Resolved mount configuration for an `up` invocation.
struct ResolvedMountConfig {
    additional_cli_mounts: Vec<MountConfig>,
    workspace_mount_consistency: Option<String>,
    mount_workspace_git_root: bool,
    mount_git_worktree_common_dir: bool,
}

/// How to resolve an existing/missing container for an `up` invocation.
///
/// Groups the container-resolution flags so they live together and keep
/// `UpContext` under the struct bool-count lint.
#[derive(Debug, Clone, Copy, Default)]
pub struct ContainerResolution {
    /// Tear down and recreate an existing container (`--rebuild` / `--remove`).
    pub remove_container: bool,
    /// Rebuild the image with `--no-cache`.
    pub build_no_cache: bool,
    /// `--expect-existing-container`: fail (not create) if none is found.
    pub expect_existing_container: bool,
}

/// Parse the CLI `--mount` and `--build-secret` flags, mapping their errors
/// into the shared boxed error type.
fn parse_cli_mounts_and_secrets(
    args: &UpArgs,
) -> Result<(Vec<MountConfig>, Vec<BuildSecret>), Box<dyn std::error::Error + Send + Sync>> {
    let additional_cli_mounts = args
        .mounts
        .mount
        .iter()
        .map(|s| parse_cli_mount(s))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;

    let build_secrets = args
        .build
        .secrets
        .iter()
        .map(|s| super::build::parse_build_secret(s))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;

    Ok((additional_cli_mounts, build_secrets))
}

/// Build the lifecycle gate from the parity flags + the resolved `waitFor`
/// phase. The single place the four lifecycle flags are turned into the gate
/// consumed by the orchestrator and compose paths.
fn build_lifecycle_gate(args: &UpArgs, config: &serde_json::Value) -> cella_backend::LifecycleGate {
    cella_backend::LifecycleGate::new(
        cella_backend::WaitForPhase::from_config(config),
        args.lifecycle.skip_post_create,
        cella_backend::StopAfter {
            skip_non_blocking: args.lifecycle.skip_non_blocking_commands,
            prebuild: args.lifecycle.prebuild,
        },
        args.config_inputs.skip_post_attach,
    )
}

/// Holds resolved state for an `up` invocation, shared across all code paths.
pub struct UpContext {
    pub(crate) resolved: ResolvedConfig,
    pub client: Box<dyn ContainerBackend>,
    pub container_nm: String,
    pub(crate) remote_env: Vec<String>,
    /// CLI `--remote-env` entries (lifecycle-only; config `remoteEnv` wins).
    pub(crate) cli_remote_env: Vec<String>,
    workspace_folder_from_config: Option<String>,
    default_workspace_folder: String,
    pub(crate) progress: crate::progress::Progress,
    pub(crate) output: OutputFormat,
    /// Container-resolution flags (rebuild / no-cache / expect-existing).
    pub(crate) resolution: ContainerResolution,
    pub(crate) skip_checksum: bool,
    /// Image pull policy (e.g. "always").
    pub(crate) pull_policy: Option<String>,
    /// Extra Docker labels to merge into the container (e.g., worktree labels).
    extra_labels: std::collections::HashMap<String, String>,
    /// CLI `--id-label` values (`key=value`): set on create, AND-matched on find.
    id_labels: Vec<String>,
    /// Network rule enforcement policy.
    pub(crate) network_rules: NetworkRulePolicy,
    /// Docker host override (forwarded to daemon registration).
    docker_host: Option<String>,
    /// Docker Compose profiles to activate.
    pub(crate) compose_profiles: Vec<String>,
    /// Extra env-file paths for Docker Compose.
    pub(crate) compose_env_files: Vec<PathBuf>,
    /// Pull policy for Docker Compose services.
    pub(crate) compose_pull_policy: Option<String>,
    /// `BuildKit` secrets for image builds.
    build_secrets: Vec<BuildSecret>,
    /// Secrets injected into lifecycle commands as env vars (runtime-only).
    lifecycle_secrets: Vec<String>,
    /// Extra Docker networks to connect after container start (before lifecycle hooks).
    pub(crate) extra_networks: Vec<String>,
    mount_config: ResolvedMountConfig,
    /// Resolved user env probe type (config value or CLI default).
    default_user_env_probe: cella_env::user_env_probe::UserEnvProbe,
    /// Lifecycle phase gate built from the `--skip-*` / `--prebuild` flags.
    pub(crate) lifecycle_gate: cella_backend::LifecycleGate,
    /// `docker` CLI binary path (`--docker-path`).
    docker_path: Option<String>,
    /// Standalone `docker-compose` (V1) binary (`--docker-compose-path`).
    docker_compose_path: Option<String>,
    /// CLI `--cache-from` images appended to the base build's cache sources.
    cache_from: Vec<String>,
    /// CLI `--cache-to` cache export (`BuildKit` only).
    cache_to: Option<String>,
    /// Whether `BuildKit`/buildx may be used (`--buildkit auto`); `false` for
    /// `--buildkit never`.
    use_buildkit: bool,
    /// `--gpu-availability` policy.
    gpu_availability: cella_backend::GpuAvailability,
    /// `--update-remote-user-uid-default` policy.
    update_remote_user_uid_default: cella_backend::UpdateRemoteUserUidDefault,
    /// `--omit-config-remote-env-from-metadata`: strip `remoteEnv` from the
    /// `devcontainer.metadata` label.
    omit_remote_env_from_metadata: bool,
    /// Dotfiles install inputs (`--dotfiles-*`). `repository` is normalized
    /// (owner/repo shorthand expanded) at construction so both the
    /// single-container and compose paths receive a clone-ready value.
    pub(crate) dotfiles: cella_orchestrator::config::DotfilesConfig,
}

/// Normalize a `--dotfiles-repository` value to a clone-ready form.
///
/// Mirrors the official tool (`dotfiles.ts:27-29`): a bare `owner/repo`
/// shorthand (no `:` and not a `/`, `./`, or `../` path) expands to a full
/// GitHub HTTPS URL. Full URLs (`https://`, `git@host:`, `ssh://`) and local
/// paths pass through unchanged. The `:` guard makes this idempotent.
fn normalize_dotfiles_repository(repo: &str) -> String {
    let is_path = repo.starts_with('/') || repo.starts_with("./") || repo.starts_with("../");
    let is_shorthand = !repo.contains(':') && !is_path;
    if is_shorthand {
        format!("https://github.com/{repo}.git")
    } else {
        repo.to_string()
    }
}

/// Build the orchestrator [`cella_orchestrator::config::DotfilesConfig`] from
/// the CLI dotfiles args, normalizing the repository shorthand once.
///
/// An empty `--dotfiles-repository ""` is treated as unset (matching the
/// official tool's `if (!repository) return`), so it never triggers an install.
fn build_dotfiles_config(args: &UpDotfilesArgs) -> cella_orchestrator::config::DotfilesConfig {
    cella_orchestrator::config::DotfilesConfig {
        repository: args
            .repository
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(normalize_dotfiles_repository),
        install_command: args.install_command.clone(),
        target_path: args.target_path.clone(),
    }
}

/// Parse the `--secrets-file` into lifecycle secret `KEY=VALUE` entries, or an
/// empty list when no secrets file was given.
fn resolve_lifecycle_secrets(
    args: &UpArgs,
) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
    args.build
        .secrets_file
        .as_ref()
        .map_or_else(|| Ok(Vec::new()), |path| parse_secrets_file(path))
}

/// Map the clap `--buildkit` enum to a resolved "may use `BuildKit`" boolean.
const fn buildkit_enabled(mode: BuildKitMode) -> bool {
    !matches!(mode, BuildKitMode::Never)
}

/// Map the clap `--gpu-availability` enum to the backend policy.
const fn map_gpu_availability(g: GpuAvailability) -> cella_backend::GpuAvailability {
    match g {
        GpuAvailability::All => cella_backend::GpuAvailability::All,
        GpuAvailability::Detect => cella_backend::GpuAvailability::Detect,
        GpuAvailability::None => cella_backend::GpuAvailability::None,
    }
}

/// Map the clap `--update-remote-user-uid-default` enum to the backend policy.
const fn map_uid_default(
    u: UpdateRemoteUserUidDefault,
) -> cella_backend::UpdateRemoteUserUidDefault {
    match u {
        UpdateRemoteUserUidDefault::Never => cella_backend::UpdateRemoteUserUidDefault::Never,
        UpdateRemoteUserUidDefault::On => cella_backend::UpdateRemoteUserUidDefault::On,
        UpdateRemoteUserUidDefault::Off => cella_backend::UpdateRemoteUserUidDefault::Off,
    }
}

/// Whether this `up` runs without a workspace folder.
///
/// Mirrors the official CLI's defaulting gate (`dcspec.ts:154-157`):
/// `workspace-folder` defaults to cwd ONLY when none of `--workspace-folder`,
/// `--id-label`, or `--override-config` is given. When `--id-label` or
/// `--override-config` is supplied without `--workspace-folder`, the official
/// tool leaves `workspaceFolder` undefined → no workspace bind-mount, the
/// container is found purely by id-label (or driven by override-config), and
/// there is no devcontainer.json discovery from cwd. cwd is still used as a
/// nominal root for path substitution and labels.
const fn no_workspace_requested(args: &UpArgs) -> bool {
    args.workspace_folder.is_none()
        && (!args.config_inputs.id_label.is_empty() || args.config_inputs.override_config.is_some())
}

/// Disable the workspace bind-mount by setting `workspaceMount` to the empty
/// string when it is not already present.
///
/// `map_workspace_mount` returns `None` for an empty `workspaceMount`
/// (`config_map/mounts.rs`), so injecting it here reproduces the official
/// no-workspace path's "no workspace mount" behavior without touching the
/// orchestrator's mount construction.
///
/// Divergence (documented, intentional per task scope): "inject only if
/// absent" means an `--override-config` document that itself sets a *non-empty*
/// `workspaceMount`, run without `--workspace-folder`, keeps that mount —
/// whereas the official CLI produces no workspace mount regardless. This is the
/// obscure case the task scoped to "if not already set".
fn inject_skip_workspace_mount(config: &mut serde_json::Value) {
    if let Some(obj) = config.as_object_mut()
        && !obj.contains_key("workspaceMount")
    {
        obj.insert("workspaceMount".to_string(), json!(""));
    }
}

/// Merge a container's `devcontainer.metadata` label (a JSON array of config
/// fragments) into a single devcontainer-config object, scalars last-wins.
///
/// Used by the id-label-only no-workspace path to source the config from the
/// found container when there is no `--override-config` and no cwd
/// devcontainer.json. Only the keys cella needs downstream (name, users,
/// `workspaceFolder`, remote/container env) are lifted; the full lifecycle is
/// re-read from the container by the orchestrator's reuse path.
fn config_from_metadata_label(metadata_json: &str) -> serde_json::Value {
    let entries: Vec<serde_json::Value> = serde_json::from_str(metadata_json).unwrap_or_default();
    let mut merged = serde_json::Map::new();
    for entry in &entries {
        if let Some(obj) = entry.as_object() {
            for (k, v) in obj {
                merged.insert(k.clone(), v.clone());
            }
        }
    }
    serde_json::Value::Object(merged)
}

/// Build a [`ResolvedConfig`] for the id-label-only no-workspace path from a
/// container found by `--id-label`.
///
/// The config content comes from the container's `devcontainer.metadata` label
/// (merged) with the workspace mount disabled; `workspace_root` is the nominal
/// cwd (used for substitution/labels only). The recorded `config_path` is the
/// container's `dev.cella.config_path` label when present, else a synthetic
/// path under the nominal root.
fn resolved_from_container(
    container: &cella_backend::ContainerInfo,
    nominal_root: &Path,
) -> ResolvedConfig {
    let mut config = container
        .labels
        .get("devcontainer.metadata")
        .map_or_else(|| json!({}), |m| config_from_metadata_label(m));
    inject_skip_workspace_mount(&mut config);

    let config_path = container.labels.get("dev.cella.config_path").map_or_else(
        || nominal_root.join(".devcontainer/devcontainer.json"),
        PathBuf::from,
    );

    resolve::from_config_value(config, nominal_root, config_path)
}

impl UpContext {
    pub(crate) async fn new(
        args: &UpArgs,
        progress: crate::progress::Progress,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let cwd = crate::commands::resolve_workspace_folder(args.workspace_folder.as_deref())?;
        let no_workspace = no_workspace_requested(args);

        // 1. Resolve config and connect to the backend. The no-workspace path
        // (official `dcspec.ts:155`) does not discover a devcontainer.json from
        // cwd: with --override-config the override supplies the content; with
        // only --id-label the config comes from the found container's metadata.
        // `resolve_no_workspace_config` owns the connect so the common path
        // still resolves config first (its discovery error surfaces before any
        // Docker connect), while the id-label-only path connects up front to
        // find the container.
        let (mut resolved, client) = progress
            .run_step(
                "Resolving devcontainer configuration...",
                Self::resolve_no_workspace_config(args, &cwd, no_workspace),
            )
            .await?;

        if no_workspace {
            inject_skip_workspace_mount(&mut resolved.config);
        }

        for w in &resolved.warnings {
            warn!("{}", w.message);
        }

        // Merge CLI --additional-features into the config's features (config
        // wins on collision). This flows into the image cache digest and
        // feature resolution, so the feature set and cache key both reflect it.
        if let Some(ref additional) = args.config_inputs.additional_features {
            super::features::resolve::merge_additional_features(&mut resolved.config, additional)
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
        }

        let config = &resolved.config;
        let config_name = resolved.name();

        let container_nm = container_name(&resolved.workspace_root, config_name);
        let remote_env = map_env_object(config.get("remoteEnv"));
        let workspace_folder_from_config = config
            .get("workspaceFolder")
            .and_then(|v| v.as_str())
            .map(String::from);
        let host_mount_folder = cella_git::find_git_root_folder(
            &resolved.workspace_root,
            args.mounts.mount_workspace_git_root,
        );
        let default_workspace_folder =
            compute_default_workspace_folder(&resolved.workspace_root, &host_mount_folder);

        let lifecycle_secrets = resolve_lifecycle_secrets(args)?;

        let (additional_cli_mounts, build_secrets) = parse_cli_mounts_and_secrets(args)?;

        let cella_cfg = cella_config::CellaConfig::load(&resolved.workspace_root, Some(&resolved))?;

        let lifecycle_gate = build_lifecycle_gate(args, &resolved.config);

        Ok(Self {
            resolved,
            client,
            container_nm,
            remote_env,
            cli_remote_env: args.config_inputs.remote_env.clone(),
            workspace_folder_from_config,
            default_workspace_folder,
            progress,
            output: args.output.clone(),
            resolution: ContainerResolution {
                remove_container: args.build.rebuild || args.build.remove_existing_container,
                build_no_cache: args.build.build_no_cache || cella_cfg.cli.build.no_cache,
                expect_existing_container: args.config_inputs.expect_existing_container,
            },
            skip_checksum: args.skip_checksum || cella_cfg.cli.skip_checksum,
            pull_policy: args
                .build
                .pull
                .as_ref()
                .map(ImagePullPolicy::as_str)
                .map(String::from),
            extra_labels: std::collections::HashMap::new(),
            id_labels: args.config_inputs.id_label.clone(),
            network_rules: if args.no_network_rules || cella_cfg.cli.no_network_rules {
                NetworkRulePolicy::Skip
            } else {
                NetworkRulePolicy::Enforce
            },
            docker_host: effective_docker_host(&args.backend),
            compose_profiles: args.profile.clone(),
            compose_env_files: args.env_file.clone(),
            compose_pull_policy: args.pull_policy.as_ref().map(|p| p.as_str().to_string()),
            build_secrets,
            lifecycle_secrets,
            extra_networks: Vec::new(),
            mount_config: ResolvedMountConfig {
                additional_cli_mounts,
                workspace_mount_consistency: Some(
                    args.mounts.workspace_mount_consistency.as_str().to_string(),
                ),
                mount_workspace_git_root: args.mounts.mount_workspace_git_root,
                mount_git_worktree_common_dir: args.mounts.mount_git_worktree_common_dir,
            },
            default_user_env_probe: args.default_user_env_probe,
            lifecycle_gate,
            docker_path: args.build.docker_path.clone(),
            docker_compose_path: args.build.docker_compose_path.clone(),
            cache_from: args.build.cache_from.clone(),
            cache_to: args.build.cache_to.clone(),
            use_buildkit: buildkit_enabled(args.build.buildkit),
            gpu_availability: map_gpu_availability(args.compat.gpu_availability),
            update_remote_user_uid_default: map_uid_default(
                args.compat.update_remote_user_uid_default,
            ),
            omit_remote_env_from_metadata: args.result.omit_config_remote_env_from_metadata,
            dotfiles: build_dotfiles_config(&args.dotfiles),
        })
    }

    /// Resolve the devcontainer config for `new()`, honoring the no-workspace
    /// path (official `dcspec.ts:154-157`).
    ///
    /// - Normal path (a workspace folder is in play): discover/read from cwd as
    ///   before, with `--config` / `--override-config` applied.
    /// - No-workspace + `--override-config`: the override supplies the content;
    ///   `config_with_override` already falls back gracefully when cwd has no
    ///   devcontainer.json, so cwd stays the nominal root and no discovery
    ///   error is raised.
    /// - No-workspace + only `--id-label`: source the config from the container
    ///   found by id-label (its `devcontainer.metadata` label). If no container
    ///   matches, error like the official tool (configContainer.ts:54).
    ///
    /// Returns the resolved config and the connected backend client. The
    /// id-label-only branch must connect before resolving (to find the
    /// container); every other path resolves config first so a missing-config
    /// error surfaces before any Docker connect, preserving the common path's
    /// error ordering.
    async fn resolve_no_workspace_config(
        args: &UpArgs,
        cwd: &Path,
        no_workspace: bool,
    ) -> Result<(ResolvedConfig, Box<dyn ContainerBackend>), Box<dyn std::error::Error + Send + Sync>>
    {
        let id_label_only = no_workspace && args.config_inputs.override_config.is_none();
        if id_label_only {
            // DOCKER-ONLY GAP: this branch (find-by-id-label, read config from
            // the found container's metadata, then start + run lifecycle via the
            // orchestrator's existing reuse path) requires a live engine and a
            // container created by an earlier `up`. It is exercised against
            // Docker, not in unit tests; the pure pieces (gate, mount-skip
            // injection, metadata→config parse, `resolved_from_container`) are
            // unit-tested. The config supplied here disables the workspace mount
            // and the orchestrator re-finds the same container by id-label.
            let client = args.backend.resolve_client().await?;
            client.ping().await?;
            let found = client
                .find_container_by_labels(&args.config_inputs.id_label)
                .await?;
            let Some(container) = found else {
                return Err("No dev container config and no workspace found.".into());
            };
            return Ok((resolved_from_container(&container, cwd), client));
        }

        // Override-config and normal paths both go through `config_with_override`.
        // With `--override-config` the override file supplies the content and
        // the cwd-discovery error is already avoided by its internal fallback.
        // Resolve config BEFORE connecting so the common path's discovery error
        // is reported first.
        let resolved = resolve::config_with_override(
            cwd,
            args.config.as_deref(),
            args.config_inputs.override_config.as_deref(),
        )?;
        let client = args.backend.resolve_client().await?;
        client.ping().await?;
        Ok((resolved, client))
    }

    /// Create an `UpContext` for a workspace path (used by `cella branch`).
    ///
    /// Unlike `new()`, this does not take `UpArgs` — it accepts the workspace
    /// path and options directly. Always sets `remove_container` and
    /// `build_no_cache` to false.
    pub async fn for_workspace(
        workspace_path: &Path,
        backend_args: &crate::backend::BackendArgs,
        extra_labels: std::collections::HashMap<String, String>,
        progress: crate::progress::Progress,
        output: OutputFormat,
        default_user_env_probe: cella_env::user_env_probe::UserEnvProbe,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let cwd = workspace_path
            .canonicalize()
            .unwrap_or_else(|_| workspace_path.to_path_buf());

        let resolved = progress
            .run_step("Resolving devcontainer configuration...", async {
                resolve::config(&cwd, None)
            })
            .await?;

        for w in &resolved.warnings {
            warn!("{}", w.message);
        }

        let config = &resolved.config;
        let config_name = resolved.name();

        let client = backend_args.resolve_client().await?;
        client.ping().await?;

        let container_nm = container_name(&resolved.workspace_root, config_name);
        let remote_env = map_env_object(config.get("remoteEnv"));
        let workspace_folder_from_config = config
            .get("workspaceFolder")
            .and_then(|v| v.as_str())
            .map(String::from);
        let host_mount_folder = cella_git::find_git_root_folder(&resolved.workspace_root, true);
        let default_workspace_folder =
            compute_default_workspace_folder(&resolved.workspace_root, &host_mount_folder);

        Ok(Self {
            resolved,
            client,
            container_nm,
            remote_env,
            cli_remote_env: Vec::new(),
            workspace_folder_from_config,
            default_workspace_folder,
            progress,
            output,
            resolution: ContainerResolution::default(),
            skip_checksum: false,
            pull_policy: None,
            extra_labels,
            id_labels: Vec::new(),
            network_rules: NetworkRulePolicy::Enforce,
            docker_host: effective_docker_host(backend_args),
            compose_profiles: Vec::new(),
            compose_env_files: Vec::new(),
            compose_pull_policy: None,
            build_secrets: vec![],
            lifecycle_secrets: Vec::new(),
            extra_networks: Vec::new(),
            mount_config: ResolvedMountConfig {
                additional_cli_mounts: Vec::new(),
                workspace_mount_consistency: None,
                mount_workspace_git_root: true,
                mount_git_worktree_common_dir: false,
            },
            default_user_env_probe,
            lifecycle_gate: cella_backend::LifecycleGate::default(),
            docker_path: None,
            docker_compose_path: None,
            cache_from: Vec::new(),
            cache_to: None,
            // Branch/auto-up uses the default toolchain: BuildKit auto, default
            // GPU detection, and the standard UID-update default.
            use_buildkit: true,
            gpu_availability: cella_backend::GpuAvailability::Detect,
            update_remote_user_uid_default: cella_backend::UpdateRemoteUserUidDefault::On,
            // Branch/auto-up keeps the full metadata label (no flag plumbed).
            omit_remote_env_from_metadata: false,
            // Branch/auto-up never installs dotfiles (no --dotfiles-* flags).
            dotfiles: cella_orchestrator::config::DotfilesConfig::default(),
        })
    }

    pub(crate) const fn config(&self) -> &serde_json::Value {
        &self.resolved.config
    }

    pub(crate) fn is_compose(&self) -> bool {
        self.config().get("dockerComposeFile").is_some()
    }

    pub(crate) fn workspace_folder(&self) -> Option<&str> {
        self.workspace_folder_from_config.as_deref()
    }

    /// Build/backend tuning for the compose `up` path.
    pub(crate) fn compose_build_tuning(
        &self,
    ) -> cella_orchestrator::compose_up::ComposeBuildTuning {
        cella_orchestrator::compose_up::ComposeBuildTuning {
            docker_path: self.docker_path.clone(),
            docker_compose_path: self.docker_compose_path.clone(),
            use_buildkit: self.use_buildkit,
        }
    }

    pub(crate) const fn gpu_availability(&self) -> cella_backend::GpuAvailability {
        self.gpu_availability
    }

    pub(crate) const fn update_remote_user_uid_default(
        &self,
    ) -> cella_backend::UpdateRemoteUserUidDefault {
        self.update_remote_user_uid_default
    }

    pub(crate) const fn omit_remote_env_from_metadata(&self) -> bool {
        self.omit_remote_env_from_metadata
    }

    pub(crate) fn probe_type(&self) -> cella_env::user_env_probe::UserEnvProbe {
        self.config()
            .get("userEnvProbe")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse().ok())
            .unwrap_or(self.default_user_env_probe)
    }

    /// Register the container with the daemon for port management.
    pub(crate) async fn register_with_daemon(&self, container_id: &str) {
        let config = self.config();
        let container_ip = self
            .client
            .get_container_ip(container_id)
            .await
            .unwrap_or(None);

        let Some(mgmt_sock) = cella_env::paths::daemon_socket_path() else {
            return;
        };
        if !mgmt_sock.exists() {
            return;
        }

        let registration = cella_orchestrator::daemon_registration::from_devcontainer_config(
            config,
            &self.resolved.workspace_root,
            container_id,
            self.container_nm.clone(),
            container_ip,
            Some(self.client.kind().to_string()),
            self.docker_host.clone(),
        );
        match cella_daemon_client::DaemonClient::new(&mgmt_sock)
            .register_container(registration)
            .await
        {
            Ok(container_name) => {
                debug!("Container registered with daemon: {container_name}");
            }
            Err(e) => {
                warn!("Failed to register container with daemon: {e}");
            }
        }
    }

    /// Run post-create setup: env injection, credentials, Claude Code, userEnvProbe.
    pub(crate) async fn post_create_setup(
        &self,
        container_id: &str,
        remote_user: &str,
        env_fwd: &cella_env::EnvForwarding,
        settings: &cella_config::CellaConfig,
        remote_env: &[String],
    ) -> (
        Option<std::collections::HashMap<String, String>>,
        Vec<String>,
    ) {
        // Inject post-start environment forwarding
        self.progress
            .run_step(
                "Configuring environment...",
                inject_post_start(
                    self.client.as_ref(),
                    container_id,
                    &env_fwd.post_start,
                    remote_user,
                ),
            )
            .await;

        // Add /cella/bin to PATH in shell profiles so `cella` CLI is discoverable.
        inject_cella_path(self.client.as_ref(), container_id, remote_user).await;

        // Seed gh CLI credentials (first create only)
        if settings.credentials.gh {
            seed_gh_credentials(
                self.client.as_ref(),
                container_id,
                &self.resolved.workspace_root,
                remote_user,
            )
            .await;
        }

        // Detect user's shell for probing (use their actual shell, not /bin/sh)
        let shell = detect_shell(self.client.as_ref(), container_id, remote_user).await;

        // Probe user environment first so tool installs can use feature-provided PATH
        // (e.g., nvm adds /usr/local/share/nvm/current/bin via login shell profiles)
        let probe_type = self.probe_type();
        let probed_env = self
            .progress
            .run_step(
                "Running userEnvProbe...",
                probe_and_cache_user_env(
                    self.client.as_ref(),
                    container_id,
                    remote_user,
                    probe_type,
                    &shell,
                ),
            )
            .await;

        // Fix /tmp permissions (must be world-writable with sticky bit).
        // upload_files can reset /tmp to 755 via tar directory entries;
        // some base images may also lack the sticky bit.
        let _ = self
            .client
            .exec_command(
                container_id,
                &ExecOptions {
                    cmd: vec![
                        "sh".into(),
                        "-c".into(),
                        "chmod 1777 /tmp 2>/dev/null || true".into(),
                    ],
                    user: Some("root".to_string()),
                    env: None,
                    working_dir: None,
                },
            )
            .await;

        // Create home path symlink and populate plugin manifests
        if settings.tools.claude_code.forward_config {
            create_claude_home_symlink(self.client.as_ref(), container_id, remote_user).await;
            setup_plugin_manifests(self.client.as_ref(), container_id, remote_user).await;
        }

        // Install tools listed in [tools] install = [...]
        let tools_to_install =
            cella_orchestrator::tool_install::resolve_tool_names(&settings.tools.install);
        let spec = cella_orchestrator::tool_install::InstallSpec {
            settings,
            tools: &tools_to_install,
            probed_env: probed_env.as_ref(),
        };
        self.install_tools(container_id, remote_user, &shell, &spec)
            .await;

        // Re-probe after tool installation to capture PATH changes
        let final_probed = if tools_to_install.is_empty() {
            probed_env
        } else {
            self.progress
                .run_step(
                    "Updating environment cache...",
                    probe_and_cache_user_env(
                        self.client.as_ref(),
                        container_id,
                        remote_user,
                        probe_type,
                        &shell,
                    ),
                )
                .await
                .or(probed_env)
        };

        let mut lifecycle_env = final_probed.as_ref().map_or_else(
            || remote_env.to_vec(),
            |probed| cella_env::user_env_probe::merge_env(probed, remote_env),
        );
        if !self.lifecycle_secrets.is_empty() {
            lifecycle_env.extend(self.lifecycle_secrets.iter().cloned());
        }

        (final_probed, lifecycle_env)
    }

    /// Delegates to [`cella_orchestrator::tool_install::install_tools`].
    async fn install_tools(
        &self,
        container_id: &str,
        remote_user: &str,
        shell: &str,
        spec: &cella_orchestrator::tool_install::InstallSpec<'_>,
    ) {
        let (sender, renderer) = crate::progress::bridge(&self.progress);
        cella_orchestrator::tool_install::install_tools(
            self.client.as_ref(),
            container_id,
            remote_user,
            shell,
            spec,
            &sender,
        )
        .await;
        drop(sender);
        let _ = renderer.await;
    }
}

/// Result of ensuring a container is up and ready.
pub struct UpResult {
    pub container_id: String,
    pub remote_user: String,
    /// cella's granular provisioning state (`running`/`started`/`created`).
    /// Surfaced as the `state` key in the JSON envelope; the official
    /// `outcome` literal (`success`/`error`) is set separately at render time.
    pub outcome: String,
    pub workspace_folder: String,
    pub ssh_agent_proxy: Option<cella_orchestrator::SshAgentProxyStatus>,
    /// Docker Compose project name (compose path only; `None` for
    /// single-container). Emitted as `composeProjectName`.
    pub compose_project_name: Option<String>,
    /// Post-host-substitution devcontainer.json, populated only when
    /// `--include-configuration` is set. Emitted as `configuration`.
    pub configuration: Option<serde_json::Value>,
    /// Features-merged configuration, populated only when
    /// `--include-merged-configuration` is set. Emitted as `mergedConfiguration`.
    pub merged_configuration: Option<serde_json::Value>,
}

struct CliUpHooks<'a> {
    config: &'a serde_json::Value,
    workspace_root: &'a Path,
    managed_agent: bool,
    backend_kind: String,
    docker_host: Option<String>,
}

impl cella_orchestrator::up::UpHooks for CliUpHooks<'_> {
    fn daemon_env<'a>(
        &'a self,
        container_name: &'a str,
        host_gateway: &'a str,
    ) -> Pin<Box<dyn Future<Output = Vec<String>> + Send + 'a>> {
        Box::pin(async move {
            super::ensure_cella_daemon().await;
            query_daemon_env(container_name, host_gateway).await
        })
    }

    fn sync_agent_runtime<'a>(
        &'a self,
        client: &'a dyn ContainerBackend,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            super::ensure_cella_daemon().await;
            write_daemon_addr_to_volume(client).await;
        })
    }

    fn on_container_started(
        &self,
        container_id: &str,
        container_name: &str,
        container_ip: Option<&str>,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        let config = self.config;
        let workspace_root = self.workspace_root;
        let container_id = container_id.to_string();
        let container_name = container_name.to_string();
        let container_ip = container_ip.map(str::to_string);
        let managed_agent = self.managed_agent;
        let backend_kind = self.backend_kind.clone();
        let docker_host = self.docker_host.clone();
        Box::pin(async move {
            if !managed_agent {
                return;
            }

            super::ensure_cella_daemon().await;

            let Some(mgmt_sock) = cella_env::paths::daemon_socket_path() else {
                return;
            };
            if !mgmt_sock.exists() {
                return;
            }

            let registration = cella_orchestrator::daemon_registration::from_devcontainer_config(
                config,
                workspace_root,
                container_id,
                container_name,
                container_ip,
                Some(backend_kind),
                docker_host,
            );
            let _ = cella_daemon_client::DaemonClient::new(&mgmt_sock)
                .register_container(registration)
                .await;
        })
    }

    fn update_container_ip(
        &self,
        container_id: &str,
        container_ip: Option<&str>,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + '_>> {
        let container_id = container_id.to_string();
        let container_ip = container_ip.map(str::to_string);
        let managed_agent = self.managed_agent;
        Box::pin(async move {
            if !managed_agent {
                return true;
            }

            let Some(mgmt_sock) = cella_env::paths::daemon_socket_path() else {
                return false;
            };
            if !mgmt_sock.exists() {
                return false;
            }

            let req = cella_protocol::ManagementRequest::UpdateContainerIp {
                container_id: container_id.clone(),
                container_ip,
            };
            // Check if the daemon recognized the container.
            matches!(
                cella_daemon_client::send_management_request(&mgmt_sock, &req).await,
                Ok(cella_protocol::ManagementResponse::ContainerIpUpdated { .. })
            )
        })
    }

    fn on_container_stopping(
        &self,
        container_name: &str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        let container_name = container_name.to_string();
        Box::pin(async move {
            let Some(mgmt_sock) = cella_env::paths::daemon_socket_path() else {
                return;
            };
            if !mgmt_sock.exists() {
                return;
            }

            let req = cella_protocol::ManagementRequest::DeregisterContainer { container_name };
            let _ = cella_daemon_client::send_management_request(&mgmt_sock, &req).await;
        })
    }
}

impl UpContext {
    /// Ensure a container is up and ready, returning the result without printing output.
    ///
    /// This is the core logic shared by `cella up` and `cella code`.
    /// It handles existing containers (running, stopped) and creates new ones as needed.
    pub async fn ensure_up(
        &self,
        build_no_cache: bool,
        strict: &[StrictnessLevel],
    ) -> Result<UpResult, Box<dyn std::error::Error + Send + Sync>> {
        let (sender, renderer) = crate::progress::bridge(&self.progress);
        let hooks = CliUpHooks {
            config: self.config(),
            workspace_root: &self.resolved.workspace_root,
            managed_agent: self.client.capabilities().managed_agent,
            backend_kind: self.client.kind().to_string(),
            docker_host: self.docker_host.clone(),
        };
        let config = cella_orchestrator::UpConfig {
            resolved: &self.resolved,
            container_name: &self.container_nm,
            remote_env: &self.remote_env,
            cli_remote_env: &self.cli_remote_env,
            workspace_folder_from_config: self.workspace_folder(),
            default_workspace_folder: &self.default_workspace_folder,
            extra_labels: &self.extra_labels,
            id_labels: &self.id_labels,
            image_strategy: if build_no_cache {
                cella_orchestrator::ImageStrategy::RebuildNoCache
            } else if self.resolution.remove_container {
                cella_orchestrator::ImageStrategy::Rebuild
            } else {
                cella_orchestrator::ImageStrategy::Cached
            },
            remove_existing_container: self.resolution.remove_container,
            skip_checksum: self.skip_checksum,
            host_requirement_policy: if strict
                .iter()
                .any(|s| matches!(s, StrictnessLevel::HostRequirements | StrictnessLevel::All))
            {
                cella_orchestrator::HostRequirementPolicy::Error
            } else {
                cella_orchestrator::HostRequirementPolicy::Warn
            },
            network_rule_policy: self.network_rules,
            pull_policy: self.pull_policy.as_deref(),
            build_secrets: self.build_secrets.clone(),
            extra_networks: self.extra_networks.clone(),
            mount_flags: cella_orchestrator::MountFlags {
                additional_cli_mounts: &self.mount_config.additional_cli_mounts,
                workspace_mount_consistency: self
                    .mount_config
                    .workspace_mount_consistency
                    .as_deref(),
                mount_workspace_git_root: self.mount_config.mount_workspace_git_root,
                mount_git_worktree_common_dir: self.mount_config.mount_git_worktree_common_dir,
            },
            lifecycle_secrets: &self.lifecycle_secrets,
            user_env_probe: self.probe_type(),
            lifecycle_gate: self.lifecycle_gate,
            expect_existing_container: self.resolution.expect_existing_container,
            build_tuning: cella_orchestrator::BuildTuning {
                docker_path: self.docker_path.as_deref(),
                use_buildkit: self.use_buildkit,
                cli_cache_from: &self.cache_from,
                cache_to: self.cache_to.as_deref(),
            },
            gpu_availability: self.gpu_availability,
            update_remote_user_uid_default: self.update_remote_user_uid_default,
            metadata_options: cella_orchestrator::MetadataOptions {
                omit_remote_env: self.omit_remote_env_from_metadata,
            },
            dotfiles: self.dotfiles.clone(),
        };

        let result =
            cella_orchestrator::up::ensure_up(self.client.as_ref(), &config, &hooks, sender).await;

        // Drain the progress renderer on both success and error paths so
        // queued events (final step, warnings) are flushed before exit.
        let _ = renderer.await;

        let result =
            result.map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })?;

        Ok(UpResult {
            container_id: result.container_id,
            remote_user: result.remote_user,
            outcome: match result.outcome {
                cella_orchestrator::UpOutcome::Running => "running".to_string(),
                cella_orchestrator::UpOutcome::Started => "started".to_string(),
                cella_orchestrator::UpOutcome::Created => "created".to_string(),
            },
            workspace_folder: result.workspace_folder,
            ssh_agent_proxy: result.ssh_agent_proxy,
            // Single-container path has no compose project; the
            // include-* config keys are populated by the caller in
            // `execute`/`execute_branch` when the flags are set.
            compose_project_name: None,
            configuration: None,
            merged_configuration: None,
        })
    }
}

impl UpArgs {
    /// Handle `--branch`: start/restart a worktree branch's container.
    async fn execute_branch(
        &self,
        branch_name: &str,
        progress: crate::progress::Progress,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let cwd = std::env::current_dir()?;
        let repo_info = cella_git::discover(&cwd)?;
        let worktrees = cella_git::list(&repo_info.root)?;
        let wt = worktrees
            .iter()
            .find(|wt| wt.branch.as_deref() == Some(branch_name))
            .ok_or_else(|| {
                format!(
                    "No worktree for branch '{branch_name}'. \
                     Use `cella branch {branch_name}` to create one."
                )
            })?;

        let extra_labels = cella_backend::worktree_labels(branch_name, &repo_info.root);
        let mut ctx = UpContext::for_workspace(
            &wt.path,
            &self.backend,
            extra_labels,
            progress,
            self.output.resolve(),
            self.default_user_env_probe,
        )
        .await?;
        let _title_guard = crate::title::push_for_workspace(
            ctx.client.as_ref(),
            &wt.path,
            &ctx.container_nm,
            None,
            Some(branch_name),
            "up",
        )
        .await;
        ctx.resolution.remove_container =
            self.build.rebuild || self.build.remove_existing_container;
        ctx.resolution.build_no_cache = self.build.build_no_cache;
        ctx.skip_checksum = self.skip_checksum;
        ctx.pull_policy = self
            .build
            .pull
            .as_ref()
            .map(ImagePullPolicy::as_str)
            .map(String::from);
        ctx.network_rules = if self.no_network_rules {
            NetworkRulePolicy::Skip
        } else {
            NetworkRulePolicy::Enforce
        };

        let mut result = if ctx.is_compose() {
            let progress = ctx.progress.clone();
            super::branch::run_compose_branch(
                &mut ctx,
                &repo_info.root,
                &progress,
                self.build.build_no_cache,
                &self.strict,
            )
            .await?
        } else {
            ctx.ensure_up(self.build.build_no_cache, &self.strict)
                .await?
        };
        populate_envelope_extras(&self.result, &ctx, &mut result).await?;
        output_result(&result_render_data(&ctx.output, &result));
        Ok(())
    }

    /// Entry point for `cella up`.
    ///
    /// Wraps [`Self::execute_inner`] so that, when the resolved output format
    /// is JSON (explicit `--output json` or auto-resolved because stdout is
    /// piped), a failure is reported as the official error envelope on STDOUT
    /// with exit code 1 — matching the official CLI's always-stdout result
    /// contract. In text/TTY mode the error propagates unchanged for the
    /// normal human diagnostic path.
    pub async fn execute(
        self,
        progress: crate::progress::Progress,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let resolved_format = self.output.resolve();
        // The `up` argument surface is large; box the inner future to keep
        // the wrapper's frame small (clippy::large_futures).
        match Box::pin(self.execute_inner(progress)).await {
            Ok(()) => Ok(()),
            Err(e) => {
                if matches!(resolved_format, OutputFormat::Json) {
                    output_error_result(&e.to_string());
                    std::process::exit(1);
                }
                Err(e)
            }
        }
    }

    async fn execute_inner(
        self,
        progress: crate::progress::Progress,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.acknowledge_compat_flags();

        if let Some(ref branch_name) = self.branch {
            return self.execute_branch(branch_name, progress).await;
        }

        let mut ctx = UpContext::new(&self, progress).await?;
        // Collapse `Auto` so the renderers (and the compose delegate) only
        // ever see concrete `Text`/`Json`.
        ctx.output = ctx.output.resolve();
        let _title_guard = crate::title::push_for_workspace(
            ctx.client.as_ref(),
            &ctx.resolved.workspace_root,
            &ctx.container_nm,
            None,
            None,
            "up",
        )
        .await;

        // Docker Compose branch: if dockerComposeFile is present, delegate to compose flow
        if ctx.config().get("dockerComposeFile").is_some() {
            return super::compose_up::compose_up(ctx, &self.result).await;
        }

        let mut result = ctx
            .ensure_up(self.build.build_no_cache, &self.strict)
            .await?;
        populate_envelope_extras(&self.result, &ctx, &mut result).await?;
        output_result(&result_render_data(&ctx.output, &result));
        Ok(())
    }
}

/// Populate the optional `--include-configuration` /
/// `--include-merged-configuration` envelope fields on `result`.
///
/// `configuration` is a clone of the post-host-substitution
/// devcontainer.json with a `configFilePath` URI object injected (cella's
/// resolved config does not carry it; see [`super::read_configuration`]).
///
/// KNOWN GAP: container-env `${containerEnv:...}` substitution is host-time
/// only — refs are collapsed at resolve time, not against the live container.
pub async fn populate_envelope_extras(
    flags: &UpResultArgs,
    ctx: &UpContext,
    result: &mut UpResult,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if flags.include_configuration {
        let mut cfg = ctx.config().clone();
        inject_config_file_path(&mut cfg, &ctx.resolved.config_path);
        result.configuration = Some(cfg);
    }
    if flags.include_merged_configuration {
        // KNOWN GAP: merged shape diverges from official mergeConfiguration
        // (plural lifecycle arrays, customizations Record); tracked for a
        // follow-up.
        result.merged_configuration = Some(
            super::read_configuration::resolve_merged_config(
                ctx.config(),
                &ctx.resolved.config_path,
            )
            .await?,
        );
    }
    Ok(())
}

/// Inject a `configFilePath` URI object into a cloned `configuration`,
/// matching the shape the official CLI embeds (and the one
/// [`super::read_configuration`] emits as a sibling key).
fn inject_config_file_path(config: &mut serde_json::Value, config_path: &Path) {
    let canonical = config_path
        .canonicalize()
        .unwrap_or_else(|_| config_path.to_path_buf());
    let path_str = canonical.to_string_lossy();
    if let Some(obj) = config.as_object_mut() {
        obj.insert(
            "configFilePath".to_string(),
            json!({
                "fsPath": path_str,
                "$mid": 1,
                "path": path_str,
                "scheme": "file",
            }),
        );
    }
}

/// Borrow a [`UpResult`] (plus the chosen format) as a [`UpRenderData`] with
/// the official `outcome: "success"` literal and cella's granular value as
/// `state`.
pub fn result_render_data<'a>(format: &'a OutputFormat, result: &'a UpResult) -> UpRenderData<'a> {
    UpRenderData {
        format,
        outcome: "success",
        state: &result.outcome,
        container_id: &result.container_id,
        remote_user: &result.remote_user,
        workspace_folder: &result.workspace_folder,
        ssh_agent_proxy: result.ssh_agent_proxy.as_ref(),
        compose_project_name: result.compose_project_name.as_deref(),
        configuration: result.configuration.as_ref(),
        merged_configuration: result.merged_configuration.as_ref(),
    }
}

pub fn map_env_object(value: Option<&serde_json::Value>) -> Vec<String> {
    cella_orchestrator::container_setup::map_env_object(value)
}

/// Return the effective Docker host for daemon registration.
///
/// Prefers the explicit CLI `--docker-host` flag; falls back to the
/// `DOCKER_HOST` environment variable so daemon-spawned follow-up
/// operations target the same engine.
fn effective_docker_host(args: &crate::backend::BackendArgs) -> Option<String> {
    args.docker_host
        .clone()
        .or_else(|| std::env::var("DOCKER_HOST").ok())
}

/// Everything the `up` success-result renderers need, gathered into one
/// borrow so [`output_result`] / [`render_up_result`] keep a single
/// argument (and stay clear of `clippy::too_many_arguments`).
///
/// `outcome` is the official fixed literal (`"success"`); `state` carries
/// cella's granular `running`/`started`/`created` value. The three trailing
/// `Option`s are the optional envelope keys — absent unless populated.
pub struct UpRenderData<'a> {
    pub format: &'a OutputFormat,
    pub outcome: &'a str,
    pub state: &'a str,
    pub container_id: &'a str,
    pub remote_user: &'a str,
    pub workspace_folder: &'a str,
    pub ssh_agent_proxy: Option<&'a cella_orchestrator::SshAgentProxyStatus>,
    pub compose_project_name: Option<&'a str>,
    pub configuration: Option<&'a serde_json::Value>,
    pub merged_configuration: Option<&'a serde_json::Value>,
}

pub fn output_result(data: &UpRenderData<'_>) {
    let rendered = render_up_result(data);
    match data.format {
        // `Auto` is resolved to `Text`/`Json` before reaching the renderers;
        // treat any non-Json value as the human (stderr) path.
        OutputFormat::Json => println!("{rendered}"),
        OutputFormat::Auto | OutputFormat::Text => eprint!("{rendered}"),
    }
}

/// Render the JSON error envelope and write it to STDOUT.
///
/// Mirrors the official CLI's failure result: `{ outcome: "error", message,
/// description }`. cella has no structured container error carrying
/// `containerId`/`didStopContainer`/`disallowedFeatureId`/`learnMoreUrl`, so
/// those keys are simply absent (documented gap).
pub fn output_error_result(message: &str) {
    println!("{}", render_error_result(message));
}

/// Pure formatter for the error envelope (single-line JSON, no trailing
/// newline) so the shape can be unit-tested without capturing stdout.
#[must_use]
pub fn render_error_result(message: &str) -> String {
    let output = json!({
        "outcome": "error",
        "message": message,
        "description": "An error occurred setting up the container.",
    });
    serde_json::to_string(&output).unwrap_or_default()
}

/// Pure formatter for the `cella up` success output. Returns the exact
/// bytes that `output_result` would write (Text → trailing newlines
/// included; Json → single-line, no trailing newline) so unit tests can
/// snapshot the output without capturing stderr/stdout.
#[must_use]
pub fn render_up_result(data: &UpRenderData<'_>) -> String {
    match data.format {
        OutputFormat::Auto | OutputFormat::Text => render_up_result_text(data),
        OutputFormat::Json => render_up_result_json(data),
    }
}

fn render_up_result_text(data: &UpRenderData<'_>) -> String {
    let container_id = data.container_id;
    let short_id = &container_id[..12.min(container_id.len())];
    let state = data.state;
    let workspace_folder = data.workspace_folder;
    let mut out = format!("Container {state}. ID: {short_id} Workspace: {workspace_folder}\n");
    if let Some(status) = data.ssh_agent_proxy {
        match status {
            cella_orchestrator::SshAgentProxyStatus::Bridged {
                host_endpoint,
                refcount,
            } => {
                use std::fmt::Write;
                let _ = writeln!(
                    out,
                    "ssh-agent proxy: bridged via {host_endpoint} (refcount {refcount})"
                );
            }
            cella_orchestrator::SshAgentProxyStatus::Skipped { reason } => {
                use std::fmt::Write;
                let _ = writeln!(out, "ssh-agent proxy: skipped — {reason}");
            }
        }
    }
    out
}

fn render_up_result_json(data: &UpRenderData<'_>) -> String {
    let mut output = serde_json::Map::new();
    // `outcome` is the official fixed literal; cella's granular state lives
    // under `state` so no information is lost.
    output.insert("outcome".to_string(), json!(data.outcome));
    output.insert("state".to_string(), json!(data.state));
    output.insert("containerId".to_string(), json!(data.container_id));
    if let Some(name) = data.compose_project_name {
        output.insert("composeProjectName".to_string(), json!(name));
    }
    output.insert("remoteUser".to_string(), json!(data.remote_user));
    output.insert(
        "remoteWorkspaceFolder".to_string(),
        json!(data.workspace_folder),
    );
    if let Some(status) = data.ssh_agent_proxy {
        let value = match status {
            cella_orchestrator::SshAgentProxyStatus::Bridged {
                host_endpoint,
                refcount,
            } => json!({
                "state": "bridged",
                "hostEndpoint": host_endpoint,
                "refcount": refcount,
            }),
            cella_orchestrator::SshAgentProxyStatus::Skipped { reason } => json!({
                "state": "skipped",
                "reason": reason,
            }),
        };
        output.insert("sshAgentProxy".to_string(), value);
    }
    if let Some(cfg) = data.configuration {
        output.insert("configuration".to_string(), cfg.clone());
    }
    if let Some(merged) = data.merged_configuration {
        output.insert("mergedConfiguration".to_string(), merged.clone());
    }
    serde_json::to_string(&serde_json::Value::Object(output)).unwrap_or_default()
}

/// Query the daemon for control port + auth token, returning env vars to inject.
///
/// `host_gateway` is the hostname the container uses to reach the host
/// (e.g. `"host.docker.internal"` for Docker, `"host.local"` for Apple Container).
pub async fn query_daemon_env(container_nm: &str, host_gateway: &str) -> Vec<String> {
    if let Some(mgmt_sock) = cella_env::paths::daemon_socket_path()
        && mgmt_sock.exists()
    {
        let status_resp = cella_daemon_client::send_management_request(
            &mgmt_sock,
            &cella_protocol::ManagementRequest::QueryStatus,
        )
        .await;

        if let Ok(cella_protocol::ManagementResponse::Status {
            control_port,
            control_token,
            ..
        }) = &status_resp
        {
            return vec![
                format!("CELLA_DAEMON_ADDR={host_gateway}:{control_port}"),
                format!("CELLA_DAEMON_TOKEN={control_token}"),
                format!("CELLA_CONTAINER_NAME={container_nm}"),
            ];
        }
    }
    vec![]
}

/// Inject post-start environment forwarding into a running container.
///
/// Uploads SSH config files and sets git config.
/// Never fails — individual steps log warnings and are skipped on error.
pub async fn inject_post_start(
    client: &dyn ContainerBackend,
    container_id: &str,
    post_start: &cella_env::PostStartInjection,
    remote_user: &str,
) {
    cella_orchestrator::container_setup::inject_post_start(
        client,
        container_id,
        post_start,
        remote_user,
    )
    .await;
}

/// Add `/cella/bin` to PATH in the container's shell profile.
async fn inject_cella_path(client: &dyn ContainerBackend, container_id: &str, remote_user: &str) {
    cella_orchestrator::container_setup::inject_cella_path(client, container_id, remote_user).await;
}

// ── Shared container-operation helpers (delegated to orchestrator) ─────────

/// Seed gh CLI credentials into a container.
async fn seed_gh_credentials(
    client: &dyn ContainerBackend,
    container_id: &str,
    workspace_root: &Path,
    remote_user: &str,
) {
    cella_orchestrator::container_setup::seed_gh_credentials(
        client,
        container_id,
        workspace_root,
        remote_user,
    )
    .await;
}

/// Create a symlink from the host's `.claude` path to the container's so that
/// hardcoded paths in plugin manifests resolve transparently.
async fn create_claude_home_symlink(
    client: &dyn ContainerBackend,
    container_id: &str,
    remote_user: &str,
) {
    cella_orchestrator::tool_install::create_claude_home_symlink(client, container_id, remote_user)
        .await;
}

/// Populate the tmpfs-backed `~/.claude/plugins/` directory.
async fn setup_plugin_manifests(
    client: &dyn ContainerBackend,
    container_id: &str,
    remote_user: &str,
) {
    cella_orchestrator::tool_install::setup_plugin_manifests(client, container_id, remote_user)
        .await;
}

// ── Version skew helpers ─────────────────────────────────────────────────

/// Write the `.daemon_addr` file to the shared agent volume.
///
/// Queries the daemon for its current control port and auth token, then
/// writes them to `/cella/.daemon_addr` on the volume so agents can
/// discover the daemon on startup and reconnect after restarts.
///
/// Returns `true` if the file was written successfully.
pub async fn write_daemon_addr_to_volume(client: &dyn ContainerBackend) -> bool {
    let Some(mgmt_sock) = cella_env::paths::daemon_socket_path() else {
        return false;
    };
    if !mgmt_sock.exists() {
        return false;
    }

    let Ok(cella_protocol::ManagementResponse::Status {
        control_port,
        control_token,
        ..
    }) = cella_daemon_client::send_management_request(
        &mgmt_sock,
        &cella_protocol::ManagementRequest::QueryStatus,
    )
    .await
    else {
        warn!("Failed to query daemon status for .daemon_addr write");
        return false;
    };

    let gateway = client.host_gateway();
    let addr = format!("{gateway}:{control_port}");
    if let Err(e) = client.write_agent_addr("", &addr, &control_token).await {
        warn!("Failed to write .daemon_addr to agent volume: {e}");
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── no-workspace gate (dcspec.ts:154-157) ──────────────────────

    fn up_args(extra: &[&str]) -> UpArgs {
        use clap::Parser;
        let mut argv = vec!["cella", "up"];
        argv.extend_from_slice(extra);
        let cli = crate::Cli::try_parse_from(argv).expect("up args parse");
        match cli.command {
            crate::commands::Command::Up(args) => args,
            _ => panic!("expected up command"),
        }
    }

    #[test]
    fn gate_false_for_plain_up() {
        // The common `cella up` (no flags) must default to cwd and mount the
        // workspace — the gate must be FALSE so the normal path is unchanged.
        assert!(!no_workspace_requested(&up_args(&[])));
    }

    #[test]
    fn gate_false_with_explicit_workspace_folder() {
        // An explicit --workspace-folder always wins; defaulting logic is off.
        assert!(!no_workspace_requested(&up_args(&[
            "--workspace-folder",
            "/some/dir"
        ])));
    }

    #[test]
    fn gate_false_when_workspace_folder_paired_with_id_label() {
        assert!(!no_workspace_requested(&up_args(&[
            "--workspace-folder",
            "/some/dir",
            "--id-label",
            "foo=bar",
        ])));
    }

    #[test]
    fn gate_true_with_id_label_only() {
        assert!(no_workspace_requested(&up_args(&["--id-label", "foo=bar"])));
    }

    #[test]
    fn gate_true_with_override_config_only() {
        assert!(no_workspace_requested(&up_args(&[
            "--override-config",
            "/tmp/o.json"
        ])));
    }

    #[test]
    fn gate_true_with_both_id_label_and_override_config() {
        assert!(no_workspace_requested(&up_args(&[
            "--id-label",
            "foo=bar",
            "--override-config",
            "/tmp/o.json",
        ])));
    }

    // ── workspace-mount skip injection ─────────────────────────────

    #[test]
    fn inject_skip_mount_sets_empty_when_absent() {
        let mut config = serde_json::json!({"image": "ubuntu"});
        inject_skip_workspace_mount(&mut config);
        assert_eq!(
            config.get("workspaceMount").and_then(|v| v.as_str()),
            Some("")
        );
    }

    #[test]
    fn inject_skip_mount_preserves_existing() {
        // "inject only if absent": a config that already sets workspaceMount is
        // left untouched (documented divergence for non-empty override mounts).
        let mut config = serde_json::json!({"workspaceMount": "type=bind,source=/h,target=/c"});
        inject_skip_workspace_mount(&mut config);
        assert_eq!(
            config.get("workspaceMount").and_then(|v| v.as_str()),
            Some("type=bind,source=/h,target=/c")
        );
    }

    #[test]
    fn inject_skip_mount_noop_on_non_object() {
        let mut config = serde_json::json!("not an object");
        inject_skip_workspace_mount(&mut config);
        assert!(config.is_string());
    }

    // The empty-string `workspaceMount` → no-mount contract that this injection
    // relies on is verified by `map_workspace_mount_explicitly_disabled` in
    // `cella-config`'s `config_map/mounts.rs` (the consumer of the injected
    // value). The full no-mount/attach/start behavior of the no-workspace path
    // is exercised only against a real Docker engine and is documented as a
    // Docker-only gap.

    // ── metadata-label → config (id-label-only path) ───────────────

    #[test]
    fn config_from_metadata_merges_entries_last_wins() {
        let meta = serde_json::json!([
            {"remoteUser": "root", "workspaceFolder": "/workspaces/old"},
            {"id": "ghcr.io/x/y"},
            {"remoteUser": "vscode", "workspaceFolder": "/workspaces/app"}
        ])
        .to_string();
        let config = config_from_metadata_label(&meta);
        assert_eq!(
            config.get("remoteUser").and_then(|v| v.as_str()),
            Some("vscode")
        );
        assert_eq!(
            config.get("workspaceFolder").and_then(|v| v.as_str()),
            Some("/workspaces/app")
        );
    }

    #[test]
    fn config_from_metadata_handles_garbage() {
        assert_eq!(
            config_from_metadata_label("not json"),
            serde_json::json!({})
        );
        assert_eq!(config_from_metadata_label("[]"), serde_json::json!({}));
    }

    #[test]
    fn resolved_from_container_skips_mount_and_reads_workspace_folder() {
        let labels = [
            (
                "devcontainer.metadata".to_string(),
                serde_json::json!([{"workspaceFolder": "/workspaces/app", "remoteUser": "vscode"}])
                    .to_string(),
            ),
            (
                "dev.cella.config_path".to_string(),
                "/home/me/app/.devcontainer/devcontainer.json".to_string(),
            ),
        ]
        .into_iter()
        .collect();
        let container = cella_backend::ContainerInfo {
            id: "abc".to_string(),
            name: "abc".to_string(),
            state: cella_backend::ContainerState::Running,
            exit_code: None,
            labels,
            config_hash: None,
            ports: Vec::new(),
            created_at: None,
            container_user: None,
            image: None,
            mounts: Vec::new(),
            backend: cella_backend::BackendKind::Docker,
        };
        let resolved = resolved_from_container(&container, Path::new("/cwd"));
        assert_eq!(
            resolved
                .config
                .get("workspaceMount")
                .and_then(|v| v.as_str()),
            Some(""),
            "no-workspace path must disable the workspace mount"
        );
        assert_eq!(
            resolved
                .config
                .get("workspaceFolder")
                .and_then(|v| v.as_str()),
            Some("/workspaces/app")
        );
        assert_eq!(resolved.workspace_root, Path::new("/cwd"));
        assert_eq!(
            resolved.config_path,
            PathBuf::from("/home/me/app/.devcontainer/devcontainer.json")
        );
    }

    // ── parse_cli_mount ────────────────────────────────────────────

    #[test]
    fn parse_cli_mount_bind() {
        let m = parse_cli_mount("type=bind,source=/host,target=/container").unwrap();
        assert_eq!(m.mount_type, "bind");
        assert_eq!(m.source, "/host");
        assert_eq!(m.target, "/container");
        assert!(!m.external);
    }

    #[test]
    fn parse_cli_mount_volume() {
        let m = parse_cli_mount("type=volume,source=mydata,target=/data").unwrap();
        assert_eq!(m.mount_type, "volume");
        assert_eq!(m.source, "mydata");
        assert_eq!(m.target, "/data");
    }

    #[test]
    fn parse_cli_mount_with_external_true() {
        let m = parse_cli_mount("type=volume,source=ext-vol,target=/ext,external=true").unwrap();
        assert!(m.external);
    }

    #[test]
    fn parse_cli_mount_with_external_false() {
        let m = parse_cli_mount("type=volume,source=vol,target=/vol,external=false").unwrap();
        assert!(!m.external);
    }

    #[test]
    fn parse_cli_mount_missing_type() {
        assert!(parse_cli_mount("source=/a,target=/b").is_err());
    }

    #[test]
    fn parse_cli_mount_missing_source() {
        assert!(parse_cli_mount("type=bind,target=/b").is_err());
    }

    #[test]
    fn parse_cli_mount_missing_target() {
        assert!(parse_cli_mount("type=bind,source=/a").is_err());
    }

    #[test]
    fn parse_cli_mount_invalid_type() {
        let err = parse_cli_mount("type=tmpfs,source=/a,target=/b").unwrap_err();
        assert!(err.contains("invalid mount type"));
    }

    #[test]
    fn parse_cli_mount_invalid_format() {
        let err = parse_cli_mount("garbage").unwrap_err();
        assert!(err.contains("invalid mount format"));
    }

    #[test]
    fn parse_cli_mount_clap_flag() {
        use clap::Parser;
        let cli = crate::Cli::try_parse_from([
            "cella",
            "up",
            "--mount",
            "type=bind,source=/a,target=/b",
            "--mount",
            "type=volume,source=vol,target=/c",
        ])
        .unwrap();
        if let crate::commands::Command::Up(args) = &cli.command {
            assert_eq!(args.mounts.mount.len(), 2);
        }
    }

    // ── map_env_object ─────────────────────────────────────────────

    #[test]
    fn map_env_object_none() {
        let result = map_env_object(None);
        assert!(result.is_empty());
    }

    #[test]
    fn map_env_object_null_value() {
        let val = serde_json::Value::Null;
        let result = map_env_object(Some(&val));
        assert!(result.is_empty());
    }

    #[test]
    fn map_env_object_with_entries() {
        let val = serde_json::json!({
            "FOO": "bar",
            "BAZ": "qux"
        });
        let result = map_env_object(Some(&val));
        assert_eq!(result.len(), 2);
        assert!(result.contains(&"FOO=bar".to_string()));
        assert!(result.contains(&"BAZ=qux".to_string()));
    }

    #[test]
    fn map_env_object_empty_object() {
        let val = serde_json::json!({});
        let result = map_env_object(Some(&val));
        assert!(result.is_empty());
    }

    #[test]
    fn map_env_object_with_null_values() {
        let val = serde_json::json!({
            "FOO": "bar",
            "SKIP": null
        });
        let result = map_env_object(Some(&val));
        // null values are typically filtered out
        assert!(result.iter().any(|e| e.starts_with("FOO=")));
    }

    // ── output_result / render_up_result helpers ───────────────────

    /// Build a minimal [`UpRenderData`] for a render test: official
    /// `outcome: "success"`, the given granular `state`, and no optional
    /// envelope keys. Callers tweak fields on the returned value as needed.
    fn render_data<'a>(
        format: &'a OutputFormat,
        state: &'a str,
        container_id: &'a str,
        ssh_agent_proxy: Option<&'a cella_orchestrator::SshAgentProxyStatus>,
    ) -> UpRenderData<'a> {
        UpRenderData {
            format,
            outcome: "success",
            state,
            container_id,
            remote_user: "vscode",
            workspace_folder: "/workspaces/test",
            ssh_agent_proxy,
            compose_project_name: None,
            configuration: None,
            merged_configuration: None,
        }
    }

    // ── output_result ──────────────────────────────────────────────

    #[test]
    fn output_result_text_mode_does_not_panic() {
        // Text mode writes to stderr, just verify it doesn't panic
        output_result(&render_data(
            &OutputFormat::Text,
            "created",
            "abcdef123456",
            None,
        ));
    }

    #[test]
    fn output_result_json_mode_does_not_panic() {
        // JSON mode writes to stdout, just verify it doesn't panic
        output_result(&render_data(
            &OutputFormat::Json,
            "created",
            "abcdef123456",
            None,
        ));
    }

    // ── render_up_result snapshots ─────────────────────────────────
    //
    // Snapshot the exact bytes that `output_result` writes so a future
    // change to the user-facing string (or its JSON shape) shows up as
    // a review-time diff rather than a silent UX regression.

    #[test]
    fn render_up_result_text_no_ssh_agent_proxy() {
        // The trailing newline is load-bearing: without it, the next
        // shell prompt would consume the status line. Snapshot the full
        // string (newline included) to lock that property in.
        let out = render_up_result(&render_data(
            &OutputFormat::Text,
            "created",
            "abcdef123456",
            None,
        ));
        insta::assert_snapshot!(
            out,
            @"Container created. ID: abcdef123456 Workspace: /workspaces/test\n"
        );
    }

    #[test]
    fn render_up_result_text_bridged_ssh_agent_proxy() {
        let status = cella_orchestrator::SshAgentProxyStatus::Bridged {
            host_endpoint: "host.docker.internal:54321".to_string(),
            refcount: 1,
        };
        let out = render_up_result(&render_data(
            &OutputFormat::Text,
            "created",
            "abcdef123456",
            Some(&status),
        ));
        // Both lines end with `\n`; final newline is load-bearing.
        insta::assert_snapshot!(out, @r"
        Container created. ID: abcdef123456 Workspace: /workspaces/test
        ssh-agent proxy: bridged via host.docker.internal:54321 (refcount 1)
        ");
    }

    #[test]
    fn render_up_result_text_skipped_ssh_agent_proxy() {
        let status = cella_orchestrator::SshAgentProxyStatus::Skipped {
            reason: "daemon socket not found".to_string(),
        };
        let out = render_up_result(&render_data(
            &OutputFormat::Text,
            "created",
            "abcdef123456",
            Some(&status),
        ));
        insta::assert_snapshot!(out, @r"
        Container created. ID: abcdef123456 Workspace: /workspaces/test
        ssh-agent proxy: skipped — daemon socket not found
        ");

        // Verify the renderer ends Text output with `\n` (separately
        // asserted because the indented multi-line snapshot above
        // normalizes whitespace and would mask a missing trailing
        // newline).
        assert!(out.ends_with('\n'));
    }

    #[test]
    fn render_up_result_text_uses_short_container_id() {
        // Long container IDs are truncated to 12 hex chars for the
        // status line — matches docker's own short-id convention.
        let out = render_up_result(&render_data(
            &OutputFormat::Text,
            "created",
            "abcdef0123456789cafef00ddeadbeef",
            None,
        ));
        insta::assert_snapshot!(
            out,
            @"Container created. ID: abcdef012345 Workspace: /workspaces/test\n"
        );
    }

    #[test]
    fn render_up_result_json_no_ssh_agent_proxy() {
        // `outcome` is the official literal; cella's granular value moves
        // to `state`.
        let out = render_up_result(&render_data(
            &OutputFormat::Json,
            "running",
            "abcdef123456",
            None,
        ));
        insta::assert_snapshot!(
            out,
            @r#"{"containerId":"abcdef123456","outcome":"success","remoteUser":"vscode","remoteWorkspaceFolder":"/workspaces/test","state":"running"}"#
        );
    }

    #[test]
    fn render_up_result_json_bridged_ssh_agent_proxy() {
        let status = cella_orchestrator::SshAgentProxyStatus::Bridged {
            host_endpoint: "host.docker.internal:54321".to_string(),
            refcount: 2,
        };
        let out = render_up_result(&render_data(
            &OutputFormat::Json,
            "started",
            "abcdef123456",
            Some(&status),
        ));
        insta::assert_snapshot!(
            out,
            @r#"{"containerId":"abcdef123456","outcome":"success","remoteUser":"vscode","remoteWorkspaceFolder":"/workspaces/test","sshAgentProxy":{"hostEndpoint":"host.docker.internal:54321","refcount":2,"state":"bridged"},"state":"started"}"#
        );
    }

    #[test]
    fn render_up_result_json_skipped_ssh_agent_proxy() {
        let status = cella_orchestrator::SshAgentProxyStatus::Skipped {
            reason: "host SSH_AUTH_SOCK unset".to_string(),
        };
        let out = render_up_result(&render_data(
            &OutputFormat::Json,
            "created",
            "abcdef123456",
            Some(&status),
        ));
        insta::assert_snapshot!(
            out,
            @r#"{"containerId":"abcdef123456","outcome":"success","remoteUser":"vscode","remoteWorkspaceFolder":"/workspaces/test","sshAgentProxy":{"reason":"host SSH_AUTH_SOCK unset","state":"skipped"},"state":"created"}"#
        );
    }

    #[test]
    fn render_up_result_json_compose_project_name() {
        // The compose path threads `composeProjectName`; single-container
        // omits it.
        let mut data = render_data(&OutputFormat::Json, "running", "abcdef123456", None);
        data.compose_project_name = Some("cella-myapp-abc12345");
        insta::assert_snapshot!(
            render_up_result(&data),
            @r#"{"composeProjectName":"cella-myapp-abc12345","containerId":"abcdef123456","outcome":"success","remoteUser":"vscode","remoteWorkspaceFolder":"/workspaces/test","state":"running"}"#
        );
    }

    #[test]
    fn render_up_result_json_includes_configuration_when_set() {
        let cfg = json!({
            "image": "ubuntu:24.04",
            "configFilePath": {
                "fsPath": "/ws/.devcontainer/devcontainer.json",
                "$mid": 1,
                "path": "/ws/.devcontainer/devcontainer.json",
                "scheme": "file"
            }
        });
        let mut data = render_data(&OutputFormat::Json, "created", "abcdef123456", None);
        data.configuration = Some(&cfg);
        let parsed: serde_json::Value = serde_json::from_str(&render_up_result(&data)).unwrap();
        assert_eq!(parsed["configuration"]["image"], json!("ubuntu:24.04"));
        assert_eq!(
            parsed["configuration"]["configFilePath"]["scheme"],
            json!("file")
        );
    }

    #[test]
    fn render_up_result_json_omits_configuration_by_default() {
        // No flag set → the key must be entirely absent (never null).
        let out = render_up_result(&render_data(
            &OutputFormat::Json,
            "created",
            "abcdef123456",
            None,
        ));
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(parsed.get("configuration").is_none());
        assert!(parsed.get("mergedConfiguration").is_none());
    }

    #[test]
    fn render_up_result_json_includes_merged_configuration_when_set() {
        let merged = json!({"image": "ubuntu:24.04", "onCreateCommand": "echo hi"});
        let mut data = render_data(&OutputFormat::Json, "created", "abcdef123456", None);
        data.merged_configuration = Some(&merged);
        let parsed: serde_json::Value = serde_json::from_str(&render_up_result(&data)).unwrap();
        assert_eq!(
            parsed["mergedConfiguration"]["image"],
            json!("ubuntu:24.04")
        );
    }

    #[test]
    fn render_error_result_shape() {
        // The error envelope carries only outcome/message/description —
        // cella has no structured ContainerError, so containerId and the
        // other partial-failure keys are absent.
        let out = render_error_result("could not resolve devcontainer.json");
        insta::assert_snapshot!(
            out,
            @r#"{"description":"An error occurred setting up the container.","message":"could not resolve devcontainer.json","outcome":"error"}"#
        );
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(parsed.get("containerId").is_none());
        assert!(parsed.get("didStopContainer").is_none());
    }

    #[test]
    fn inject_config_file_path_adds_uri_object() {
        let mut cfg = json!({"image": "ubuntu"});
        inject_config_file_path(&mut cfg, Path::new("/ws/.devcontainer/devcontainer.json"));
        assert_eq!(cfg["configFilePath"]["scheme"], json!("file"));
        assert_eq!(cfg["configFilePath"]["$mid"], json!(1));
        // fsPath and path mirror each other.
        assert_eq!(
            cfg["configFilePath"]["fsPath"],
            cfg["configFilePath"]["path"]
        );
    }

    // ── resolve_remote_user ────────────────────────────────────────

    #[test]
    fn resolve_remote_user_from_config() {
        let config = serde_json::json!({
            "remoteUser": "devuser"
        });
        let user = cella_orchestrator::container_setup::resolve_remote_user(&config, None, "root");
        assert_eq!(user, "devuser");
    }

    #[test]
    fn resolve_remote_user_container_user_fallback() {
        let config = serde_json::json!({
            "containerUser": "containeruser"
        });
        let user = cella_orchestrator::container_setup::resolve_remote_user(&config, None, "root");
        assert_eq!(user, "containeruser");
    }

    #[test]
    fn resolve_remote_user_fallback_to_default() {
        let config = serde_json::json!({});
        let user = cella_orchestrator::container_setup::resolve_remote_user(&config, None, "root");
        assert_eq!(user, "root");
    }

    #[test]
    fn resolve_remote_user_remote_user_takes_priority() {
        let config = serde_json::json!({
            "remoteUser": "remote",
            "containerUser": "container"
        });
        let user = cella_orchestrator::container_setup::resolve_remote_user(&config, None, "root");
        assert_eq!(user, "remote");
    }

    // ── UpArgs::is_text_output ─────────────────────────────────────

    #[test]
    fn up_args_text_output() {
        use clap::Parser;
        let cli = crate::Cli::try_parse_from(["cella", "up"]).unwrap();
        if let crate::commands::Command::Up(args) = &cli.command {
            assert!(args.is_text_output());
        }
    }

    #[test]
    fn up_args_json_output() {
        use clap::Parser;
        let cli = crate::Cli::try_parse_from(["cella", "up", "--output", "json"]).unwrap();
        if let crate::commands::Command::Up(args) = &cli.command {
            assert!(!args.is_text_output());
        }
    }

    // ── parse_secrets_file ────────────────────────────────────────

    #[test]
    fn parse_valid_secrets_file() {
        use std::io::Write;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        write!(f, r#"{{"API_KEY": "secret123", "DB_PASS": "hunter2"}}"#).unwrap();
        let secrets = parse_secrets_file(f.path()).unwrap();
        assert_eq!(secrets.len(), 2);
        assert!(secrets.contains(&"API_KEY=secret123".to_string()));
        assert!(secrets.contains(&"DB_PASS=hunter2".to_string()));
    }

    #[test]
    fn parse_secrets_file_empty_object() {
        use std::io::Write;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        write!(f, "{{}}").unwrap();
        let secrets = parse_secrets_file(f.path()).unwrap();
        assert!(secrets.is_empty());
    }

    #[test]
    fn parse_secrets_file_rejects_array() {
        use std::io::Write;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        write!(f, r#"["not", "an", "object"]"#).unwrap();
        let err = parse_secrets_file(f.path()).unwrap_err();
        assert!(err.to_string().contains("must contain a JSON object"));
    }

    #[test]
    fn parse_secrets_file_rejects_non_string_values() {
        use std::io::Write;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        write!(f, r#"{{"KEY": 42}}"#).unwrap();
        let err = parse_secrets_file(f.path()).unwrap_err();
        assert!(err.to_string().contains("must be a string value"));
    }

    #[test]
    fn parse_secrets_file_missing_file() {
        let err = parse_secrets_file(Path::new("/nonexistent/secrets.json")).unwrap_err();
        assert!(err.to_string().contains("Failed to read secrets file"));
    }

    #[test]
    fn parse_secrets_file_invalid_json() {
        use std::io::Write;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        write!(f, "not json").unwrap();
        let err = parse_secrets_file(f.path()).unwrap_err();
        assert!(err.to_string().contains("Invalid JSON"));
    }

    #[test]
    fn secrets_file_clap_flag() {
        use clap::Parser;
        let cli =
            crate::Cli::try_parse_from(["cella", "up", "--secrets-file", "/tmp/s.json"]).unwrap();
        if let crate::commands::Command::Up(args) = &cli.command {
            assert_eq!(args.build.secrets_file, Some(PathBuf::from("/tmp/s.json")));
        }
    }

    // ── devcontainer-CLI flag parity ───────────────────────────────
    //
    // Source of truth: devcontainers/cli `src/spec-node/devContainersSpecCLI.ts`
    // `provisionOptions` (the `up` command). Every official long flag MUST be
    // declared on `UpArgs` so that no official invocation errors with an
    // "unknown argument" — the core drop-in-replacement invariant. Re-derive
    // this list when the official CLI adds flags.
    const OFFICIAL_UP_FLAGS: &[&str] = &[
        "docker-path",
        "docker-compose-path",
        "container-data-folder",
        "container-system-data-folder",
        "workspace-folder",
        "workspace-mount-consistency",
        "gpu-availability",
        "mount-workspace-git-root",
        "mount-git-worktree-common-dir",
        "id-label",
        "config",
        "override-config",
        "log-level",
        "log-format",
        "terminal-columns",
        "terminal-rows",
        "default-user-env-probe",
        "update-remote-user-uid-default",
        "remove-existing-container",
        "build-no-cache",
        "expect-existing-container",
        "skip-post-create",
        "skip-non-blocking-commands",
        "prebuild",
        "user-data-folder",
        "mount",
        "remote-env",
        "cache-from",
        "cache-to",
        "buildkit",
        "additional-features",
        "skip-feature-auto-mapping",
        "skip-post-attach",
        "dotfiles-repository",
        "dotfiles-install-command",
        "dotfiles-target-path",
        "container-session-data-folder",
        "omit-config-remote-env-from-metadata",
        "secrets-file",
        "experimental-lockfile",
        "experimental-frozen-lockfile",
        "no-lockfile",
        "frozen-lockfile",
        "omit-syntax-directive",
        "include-configuration",
        "include-merged-configuration",
    ];

    #[test]
    fn up_flag_parity() {
        use clap::CommandFactory;
        use std::collections::HashSet;

        let cli = crate::Cli::command();
        let up = cli
            .find_subcommand("up")
            .expect("`up` subcommand must exist");
        let longs: HashSet<&str> = up.get_arguments().filter_map(clap::Arg::get_long).collect();

        let missing: Vec<&&str> = OFFICIAL_UP_FLAGS
            .iter()
            .filter(|f| !longs.contains(**f))
            .collect();
        assert!(
            missing.is_empty(),
            "`up` is missing official devcontainer-CLI flags: {missing:?}"
        );
    }

    #[test]
    fn up_compat_flags_parse() {
        use clap::Parser;
        // Representative compat / behavioral flags must parse without error.
        let r = crate::Cli::try_parse_from([
            "cella",
            "up",
            "--workspace-folder",
            ".",
            "--user-data-folder",
            "/tmp/x",
            "--container-data-folder",
            "/tmp/y",
            "--container-system-data-folder",
            "/tmp/z",
            "--no-lockfile",
            "--docker-path",
            "docker",
            "--docker-compose-path",
            "docker-compose",
            "--skip-feature-auto-mapping",
            "--gpu-availability",
            "none",
            "--buildkit",
            "never",
            "--update-remote-user-uid-default",
            "off",
            "--id-label",
            "foo=bar",
            "--remote-env",
            "A=1",
            "--remote-env",
            "EMPTY=",
            "--cache-from",
            "img:cache",
            "--include-merged-configuration",
        ]);
        assert!(r.is_ok(), "compat flags should parse: {:?}", r.err());
    }

    #[test]
    fn up_flag_validators_reject_bad_input() {
        use clap::Parser;
        // id-label requires name=value (both non-empty).
        assert!(crate::Cli::try_parse_from(["cella", "up", "--id-label", "noequals"]).is_err());
        // remote-env requires a name before '='.
        assert!(crate::Cli::try_parse_from(["cella", "up", "--remote-env", "=novalue"]).is_err());
        // --no-lockfile conflicts with --frozen-lockfile.
        assert!(
            crate::Cli::try_parse_from(["cella", "up", "--no-lockfile", "--frozen-lockfile"])
                .is_err()
        );
        // terminal-columns requires terminal-rows.
        assert!(crate::Cli::try_parse_from(["cella", "up", "--terminal-columns", "80"]).is_err());
    }
}
