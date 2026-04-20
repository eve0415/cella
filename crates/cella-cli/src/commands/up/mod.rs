use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use clap::Args;
use serde_json::json;
use tracing::{debug, warn};

use super::{ComposePullPolicy, ImagePullPolicy, OutputFormat, StrictnessLevel};

use cella_backend::{BuildSecret, ContainerBackend, ExecOptions, container_name};
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

    /// Output format.
    #[arg(long, value_enum, default_value = "text")]
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
}

impl UpArgs {
    pub const fn is_text_output(&self) -> bool {
        matches!(self.output, OutputFormat::Text)
    }
}

use cella_orchestrator::NetworkRulePolicy;

/// Holds resolved state for an `up` invocation, shared across all code paths.
pub struct UpContext {
    pub(crate) resolved: ResolvedConfig,
    pub client: Box<dyn ContainerBackend>,
    pub container_nm: String,
    pub(crate) remote_env: Vec<String>,
    workspace_folder_from_config: Option<String>,
    default_workspace_folder: String,
    pub(crate) progress: crate::progress::Progress,
    pub(crate) output: OutputFormat,
    pub(crate) remove_container: bool,
    pub(crate) build_no_cache: bool,
    pub(crate) skip_checksum: bool,
    /// Image pull policy (e.g. "always").
    pub(crate) pull_policy: Option<String>,
    /// Extra Docker labels to merge into the container (e.g., worktree labels).
    extra_labels: std::collections::HashMap<String, String>,
    /// Network rule enforcement policy.
    network_rules: NetworkRulePolicy,
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
}

impl UpContext {
    pub(crate) async fn new(
        args: &UpArgs,
        progress: crate::progress::Progress,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let cwd = crate::commands::resolve_workspace_folder(args.workspace_folder.as_deref())?;

        let remove_container = args.build.rebuild || args.build.remove_existing_container;

        // 1. Resolve config
        let resolved = progress
            .run_step("Resolving devcontainer configuration...", async {
                resolve::config(&cwd, args.config.as_deref())
            })
            .await?;

        for w in &resolved.warnings {
            warn!("{}", w.message);
        }

        let config = &resolved.config;
        let config_name = config.get("name").and_then(|v| v.as_str());

        // 2. Connect to backend
        let client = args.backend.resolve_client().await?;
        client.ping().await?;

        let container_nm = container_name(&resolved.workspace_root, config_name);
        let remote_env = map_env_object(config.get("remoteEnv"));
        let workspace_folder_from_config = config
            .get("workspaceFolder")
            .and_then(|v| v.as_str())
            .map(String::from);
        let workspace_basename = resolved.workspace_root.file_name().map_or_else(
            || "workspace".to_string(),
            |n| n.to_string_lossy().to_string(),
        );
        let default_workspace_folder = format!("/workspaces/{workspace_basename}");

        let build_secrets = args
            .build
            .secrets
            .iter()
            .map(|s| super::build::parse_build_secret(s))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;

        Ok(Self {
            resolved,
            client,
            container_nm,
            remote_env,
            workspace_folder_from_config,
            default_workspace_folder,
            progress,
            output: args.output.clone(),
            remove_container,
            build_no_cache: args.build.build_no_cache,
            skip_checksum: args.skip_checksum,
            pull_policy: args
                .build
                .pull
                .as_ref()
                .map(ImagePullPolicy::as_str)
                .map(String::from),
            extra_labels: std::collections::HashMap::new(),
            network_rules: if args.no_network_rules {
                NetworkRulePolicy::Skip
            } else {
                NetworkRulePolicy::Enforce
            },
            docker_host: effective_docker_host(&args.backend),
            compose_profiles: args.profile.clone(),
            compose_env_files: args.env_file.clone(),
            compose_pull_policy: args.pull_policy.as_ref().map(|p| p.as_str().to_string()),
            build_secrets,
        })
    }

    /// Create an `UpContext` for a workspace path (used by `cella branch`).
    ///
    /// Unlike `new()`, this does not take `UpArgs` — it accepts the workspace
    /// path and options directly. Always sets `remove_container` and
    /// `build_no_cache` to false.
    pub async fn for_workspace(
        workspace_path: &std::path::Path,
        backend_args: &crate::backend::BackendArgs,
        extra_labels: std::collections::HashMap<String, String>,
        progress: crate::progress::Progress,
        output: OutputFormat,
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
        let config_name = config.get("name").and_then(|v| v.as_str());

        let client = backend_args.resolve_client().await?;
        client.ping().await?;

        let container_nm = container_name(&resolved.workspace_root, config_name);
        let remote_env = map_env_object(config.get("remoteEnv"));
        let workspace_folder_from_config = config
            .get("workspaceFolder")
            .and_then(|v| v.as_str())
            .map(String::from);
        let workspace_basename = resolved.workspace_root.file_name().map_or_else(
            || "workspace".to_string(),
            |n| n.to_string_lossy().to_string(),
        );
        let default_workspace_folder = format!("/workspaces/{workspace_basename}");

        Ok(Self {
            resolved,
            client,
            container_nm,
            remote_env,
            workspace_folder_from_config,
            default_workspace_folder,
            progress,
            output,
            remove_container: false,
            build_no_cache: false,
            skip_checksum: false,
            pull_policy: None,
            extra_labels,
            network_rules: NetworkRulePolicy::Enforce,
            docker_host: effective_docker_host(backend_args),
            compose_profiles: Vec::new(),
            compose_env_files: Vec::new(),
            compose_pull_policy: None,
            build_secrets: vec![],
        })
    }

    pub(crate) const fn config(&self) -> &serde_json::Value {
        &self.resolved.config
    }

    pub(crate) fn workspace_folder(&self) -> Option<&str> {
        self.workspace_folder_from_config.as_deref()
    }

    pub(crate) fn probe_type(&self) -> &str {
        self.config()
            .get("userEnvProbe")
            .and_then(|v| v.as_str())
            .unwrap_or("loginInteractiveShell")
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

        let forward_ports: Vec<u16> = config
            .get("forwardPorts")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_u64().and_then(|n| u16::try_from(n).ok()))
                    .collect()
            })
            .unwrap_or_default();

        let ports_attrs = cella_orchestrator::config_map::ports::parse_ports_attributes(config);
        let other_ports_attrs =
            cella_orchestrator::config_map::ports::parse_other_ports_attributes(config);
        let shutdown_action = config
            .get("shutdownAction")
            .and_then(|v| v.as_str())
            .map(String::from);
        let req = cella_protocol::ManagementRequest::RegisterContainer(Box::new(
            cella_protocol::ContainerRegistrationData {
                container_id: container_id.to_string(),
                container_name: self.container_nm.clone(),
                container_ip,
                ports_attributes: ports_attrs,
                other_ports_attributes: other_ports_attrs,
                forward_ports,
                shutdown_action,
                backend_kind: Some(self.client.kind().to_string()),
                docker_host: self.docker_host.clone(),
            },
        ));
        match cella_daemon::management::send_management_request(&mgmt_sock, &req).await {
            Ok(resp) => {
                debug!("Container registered with daemon: {resp:?}");
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
        settings: &cella_config::settings::Settings,
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
        let probed_env = self
            .progress
            .run_step(
                "Running userEnvProbe...",
                probe_and_cache_user_env(
                    self.client.as_ref(),
                    container_id,
                    remote_user,
                    self.probe_type(),
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

        // Install AI coding tools
        let any_tool = settings.tools.claude_code.enabled
            || settings.tools.codex.enabled
            || settings.tools.gemini.enabled;
        self.install_tools(container_id, remote_user, settings, probed_env.as_ref())
            .await;

        // Re-probe after tool installation to capture PATH changes
        // (e.g., Claude Code installer adds ~/.local/bin to shell profiles)
        let final_probed = if any_tool {
            self.progress
                .run_step(
                    "Updating environment cache...",
                    probe_and_cache_user_env(
                        self.client.as_ref(),
                        container_id,
                        remote_user,
                        self.probe_type(),
                        &shell,
                    ),
                )
                .await
                .or(probed_env)
        } else {
            probed_env
        };

        let lifecycle_env = final_probed.as_ref().map_or_else(
            || remote_env.to_vec(),
            |probed| cella_env::user_env_probe::merge_env(probed, remote_env),
        );

        (final_probed, lifecycle_env)
    }

    /// Forward config and install AI coding tools (Claude Code, Codex, Gemini).
    ///
    /// Delegates to [`cella_orchestrator::tool_install::install_tools`].
    async fn install_tools(
        &self,
        container_id: &str,
        remote_user: &str,
        settings: &cella_config::settings::Settings,
        probed_env: Option<&std::collections::HashMap<String, String>>,
    ) {
        let (sender, renderer) = crate::progress::bridge(&self.progress);
        cella_orchestrator::tool_install::install_tools(
            self.client.as_ref(),
            container_id,
            remote_user,
            settings,
            probed_env,
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
    pub outcome: String,
    pub workspace_folder: String,
    pub ssh_agent_proxy: Option<cella_orchestrator::SshAgentProxyStatus>,
}

struct CliUpHooks<'a> {
    config: &'a serde_json::Value,
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

            let forward_ports: Vec<u16> = config
                .get("forwardPorts")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_u64().and_then(|n| u16::try_from(n).ok()))
                        .collect()
                })
                .unwrap_or_default();

            let ports_attrs = cella_orchestrator::config_map::ports::parse_ports_attributes(config);
            let other_ports_attrs =
                cella_orchestrator::config_map::ports::parse_other_ports_attributes(config);
            let shutdown_action = config
                .get("shutdownAction")
                .and_then(|v| v.as_str())
                .map(String::from);

            let req = cella_protocol::ManagementRequest::RegisterContainer(Box::new(
                cella_protocol::ContainerRegistrationData {
                    container_id,
                    container_name,
                    container_ip,
                    ports_attributes: ports_attrs,
                    other_ports_attributes: other_ports_attrs,
                    forward_ports,
                    shutdown_action,
                    backend_kind: Some(backend_kind),
                    docker_host,
                },
            ));
            let _ = cella_daemon::management::send_management_request(&mgmt_sock, &req).await;
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
                cella_daemon::management::send_management_request(&mgmt_sock, &req).await,
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
            let _ = cella_daemon::management::send_management_request(&mgmt_sock, &req).await;
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
            managed_agent: self.client.capabilities().managed_agent,
            backend_kind: self.client.kind().to_string(),
            docker_host: self.docker_host.clone(),
        };
        let config = cella_orchestrator::UpConfig {
            resolved: &self.resolved,
            container_name: &self.container_nm,
            remote_env: &self.remote_env,
            workspace_folder_from_config: self.workspace_folder(),
            default_workspace_folder: &self.default_workspace_folder,
            extra_labels: &self.extra_labels,
            image_strategy: if build_no_cache {
                cella_orchestrator::ImageStrategy::RebuildNoCache
            } else if self.remove_container {
                cella_orchestrator::ImageStrategy::Rebuild
            } else {
                cella_orchestrator::ImageStrategy::Cached
            },
            remove_existing_container: self.remove_container,
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
            self.output.clone(),
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
        ctx.remove_container = self.build.rebuild || self.build.remove_existing_container;
        ctx.build_no_cache = self.build.build_no_cache;
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

        let result = ctx
            .ensure_up(self.build.build_no_cache, &self.strict)
            .await?;
        output_result(
            &ctx.output,
            &result.outcome,
            &result.container_id,
            &result.remote_user,
            &result.workspace_folder,
            result.ssh_agent_proxy.as_ref(),
        );
        Ok(())
    }

    pub async fn execute(
        self,
        progress: crate::progress::Progress,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if let Some(ref branch_name) = self.branch {
            return self.execute_branch(branch_name, progress).await;
        }

        let ctx = UpContext::new(&self, progress).await?;
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
            return super::compose_up::compose_up(ctx).await;
        }

        let result = ctx
            .ensure_up(self.build.build_no_cache, &self.strict)
            .await?;
        output_result(
            &ctx.output,
            &result.outcome,
            &result.container_id,
            &result.remote_user,
            &result.workspace_folder,
            result.ssh_agent_proxy.as_ref(),
        );
        Ok(())
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

pub fn output_result(
    format: &OutputFormat,
    outcome: &str,
    container_id: &str,
    remote_user: &str,
    workspace_folder: &str,
    ssh_agent_proxy: Option<&cella_orchestrator::SshAgentProxyStatus>,
) {
    let rendered = render_up_result(
        format,
        outcome,
        container_id,
        remote_user,
        workspace_folder,
        ssh_agent_proxy,
    );
    match format {
        OutputFormat::Text => eprint!("{rendered}"),
        OutputFormat::Json => println!("{rendered}"),
    }
}

/// Pure formatter for the `cella up` success output. Returns the exact
/// bytes that `output_result` would write (Text → trailing newlines
/// included; Json → single-line, no trailing newline) so unit tests can
/// snapshot the output without capturing stderr/stdout.
pub fn render_up_result(
    format: &OutputFormat,
    outcome: &str,
    container_id: &str,
    remote_user: &str,
    workspace_folder: &str,
    ssh_agent_proxy: Option<&cella_orchestrator::SshAgentProxyStatus>,
) -> String {
    match format {
        OutputFormat::Text => {
            let short_id = &container_id[..12.min(container_id.len())];
            let mut out =
                format!("Container {outcome}. ID: {short_id} Workspace: {workspace_folder}\n");
            if let Some(status) = ssh_agent_proxy {
                match status {
                    cella_orchestrator::SshAgentProxyStatus::Bridged {
                        proxy_socket,
                        refcount,
                    } => {
                        use std::fmt::Write;
                        let _ = writeln!(
                            out,
                            "ssh-agent proxy: bridged via {proxy_socket} (refcount {refcount})"
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
        OutputFormat::Json => {
            let mut output = serde_json::Map::new();
            output.insert("outcome".to_string(), json!(outcome));
            output.insert("containerId".to_string(), json!(container_id));
            output.insert("remoteUser".to_string(), json!(remote_user));
            output.insert("remoteWorkspaceFolder".to_string(), json!(workspace_folder));
            if let Some(status) = ssh_agent_proxy {
                let value = match status {
                    cella_orchestrator::SshAgentProxyStatus::Bridged {
                        proxy_socket,
                        refcount,
                    } => json!({
                        "state": "bridged",
                        "proxySocket": proxy_socket,
                        "refcount": refcount,
                    }),
                    cella_orchestrator::SshAgentProxyStatus::Skipped { reason } => json!({
                        "state": "skipped",
                        "reason": reason,
                    }),
                };
                output.insert("sshAgentProxy".to_string(), value);
            }
            serde_json::to_string(&serde_json::Value::Object(output)).unwrap_or_default()
        }
    }
}

/// Query the daemon for control port + auth token, returning env vars to inject.
///
/// `host_gateway` is the hostname the container uses to reach the host
/// (e.g. `"host.docker.internal"` for Docker, `"host.local"` for Apple Container).
pub async fn query_daemon_env(container_nm: &str, host_gateway: &str) -> Vec<String> {
    if let Some(mgmt_sock) = cella_env::paths::daemon_socket_path()
        && mgmt_sock.exists()
    {
        let status_resp = cella_daemon::management::send_management_request(
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
    workspace_root: &std::path::Path,
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
    }) = cella_daemon::management::send_management_request(
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

    // ── output_result ──────────────────────────────────────────────

    #[test]
    fn output_result_text_mode_does_not_panic() {
        // Text mode writes to stderr, just verify it doesn't panic
        output_result(
            &OutputFormat::Text,
            "created",
            "abcdef123456",
            "vscode",
            "/workspaces/test",
            None,
        );
    }

    #[test]
    fn output_result_json_mode_does_not_panic() {
        // JSON mode writes to stdout, just verify it doesn't panic
        output_result(
            &OutputFormat::Json,
            "created",
            "abcdef123456",
            "vscode",
            "/workspaces/test",
            None,
        );
    }

    // ── render_up_result snapshots (Phase 5) ───────────────────────
    //
    // Snapshot the exact bytes that `output_result` writes so a future
    // change to the user-facing string (or its JSON shape) shows up as
    // a review-time diff rather than a silent UX regression.

    #[test]
    fn render_up_result_text_no_ssh_agent_proxy() {
        let out = render_up_result(
            &OutputFormat::Text,
            "created",
            "abcdef123456",
            "vscode",
            "/workspaces/test",
            None,
        );
        insta::assert_snapshot!(
            out.trim_end(),
            @"Container created. ID: abcdef123456 Workspace: /workspaces/test"
        );
    }

    #[test]
    fn render_up_result_text_bridged_ssh_agent_proxy() {
        let status = cella_orchestrator::SshAgentProxyStatus::Bridged {
            proxy_socket: "/Users/me/.cella/run/ssh-agent-deadbeef.sock".to_string(),
            refcount: 1,
        };
        let out = render_up_result(
            &OutputFormat::Text,
            "created",
            "abcdef123456",
            "vscode",
            "/workspaces/test",
            Some(&status),
        );
        insta::assert_snapshot!(out.trim_end(), @r"
        Container created. ID: abcdef123456 Workspace: /workspaces/test
        ssh-agent proxy: bridged via /Users/me/.cella/run/ssh-agent-deadbeef.sock (refcount 1)
        ");
    }

    #[test]
    fn render_up_result_text_skipped_ssh_agent_proxy() {
        let status = cella_orchestrator::SshAgentProxyStatus::Skipped {
            reason: "daemon socket not found".to_string(),
        };
        let out = render_up_result(
            &OutputFormat::Text,
            "created",
            "abcdef123456",
            "vscode",
            "/workspaces/test",
            Some(&status),
        );
        insta::assert_snapshot!(out.trim_end(), @r"
        Container created. ID: abcdef123456 Workspace: /workspaces/test
        ssh-agent proxy: skipped — daemon socket not found
        ");
    }

    #[test]
    fn render_up_result_text_uses_short_container_id() {
        // Long container IDs are truncated to 12 hex chars for the
        // status line — matches docker's own short-id convention.
        let out = render_up_result(
            &OutputFormat::Text,
            "created",
            "abcdef0123456789cafef00ddeadbeef",
            "vscode",
            "/workspaces/test",
            None,
        );
        insta::assert_snapshot!(
            out.trim_end(),
            @"Container created. ID: abcdef012345 Workspace: /workspaces/test"
        );
    }

    #[test]
    fn render_up_result_json_no_ssh_agent_proxy() {
        let out = render_up_result(
            &OutputFormat::Json,
            "running",
            "abcdef123456",
            "vscode",
            "/workspaces/test",
            None,
        );
        insta::assert_snapshot!(
            out,
            @r#"{"containerId":"abcdef123456","outcome":"running","remoteUser":"vscode","remoteWorkspaceFolder":"/workspaces/test"}"#
        );
    }

    #[test]
    fn render_up_result_json_bridged_ssh_agent_proxy() {
        let status = cella_orchestrator::SshAgentProxyStatus::Bridged {
            proxy_socket: "/Users/me/.cella/run/ssh-agent-deadbeef.sock".to_string(),
            refcount: 2,
        };
        let out = render_up_result(
            &OutputFormat::Json,
            "started",
            "abcdef123456",
            "vscode",
            "/workspaces/test",
            Some(&status),
        );
        insta::assert_snapshot!(
            out,
            @r#"{"containerId":"abcdef123456","outcome":"started","remoteUser":"vscode","remoteWorkspaceFolder":"/workspaces/test","sshAgentProxy":{"proxySocket":"/Users/me/.cella/run/ssh-agent-deadbeef.sock","refcount":2,"state":"bridged"}}"#
        );
    }

    #[test]
    fn render_up_result_json_skipped_ssh_agent_proxy() {
        let status = cella_orchestrator::SshAgentProxyStatus::Skipped {
            reason: "host SSH_AUTH_SOCK unset".to_string(),
        };
        let out = render_up_result(
            &OutputFormat::Json,
            "created",
            "abcdef123456",
            "vscode",
            "/workspaces/test",
            Some(&status),
        );
        insta::assert_snapshot!(
            out,
            @r#"{"containerId":"abcdef123456","outcome":"created","remoteUser":"vscode","remoteWorkspaceFolder":"/workspaces/test","sshAgentProxy":{"reason":"host SSH_AUTH_SOCK unset","state":"skipped"}}"#
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
}
