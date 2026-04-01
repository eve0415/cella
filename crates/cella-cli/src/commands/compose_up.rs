//! Docker Compose orchestration — thin CLI wrapper over `cella_orchestrator::compose_up`.
//!
//! Implements [`ComposeUpHooks`] to provide daemon management, agent launch,
//! and post-create setup via CLI-specific code paths.

use std::future::Future;
use std::path::Path;
use std::pin::Pin;

use tracing::{debug, info, warn};

use cella_backend::{ContainerBackend, ExecOptions};
use cella_orchestrator::compose_up::{ComposeUpConfig, ComposeUpHooks, ComposeUpOutcome};

use super::up::{UpContext, output_result};

/// Run the Docker Compose orchestration flow.
///
/// Called from `UpArgs::execute()` when the resolved config contains `dockerComposeFile`.
pub async fn compose_up(ctx: UpContext) -> Result<(), Box<dyn std::error::Error>> {
    let hooks = CliComposeUpHooks { ctx: &ctx };
    let cfg = ComposeUpConfig {
        config: ctx.config(),
        config_path: &ctx.resolved.config_path,
        workspace_root: &ctx.resolved.workspace_root,
        container_name: &ctx.container_nm,
        remote_env: &ctx.remote_env,
        remove_container: ctx.remove_container,
        build_no_cache: ctx.build_no_cache,
        skip_checksum: ctx.skip_checksum,
    };

    let (sender, renderer) = crate::progress::bridge(&ctx.progress);
    let result =
        cella_orchestrator::compose_up::compose_up(ctx.client.as_ref(), &cfg, &hooks, sender)
            .await
            .map_err(|e| e.to_string());
    let _ = renderer.await;
    let result = result.map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    let outcome = match result.outcome {
        ComposeUpOutcome::Created => "created",
        ComposeUpOutcome::Running => "running",
    };
    output_result(
        &ctx.output,
        outcome,
        &result.container_id,
        &result.remote_user,
        &result.workspace_folder,
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// CLI hooks implementation
// ---------------------------------------------------------------------------

struct CliComposeUpHooks<'a> {
    ctx: &'a UpContext,
}

impl ComposeUpHooks for CliComposeUpHooks<'_> {
    fn daemon_env<'a>(
        &'a self,
        container_name: &'a str,
        host_gateway: &'a str,
    ) -> Pin<Box<dyn Future<Output = Vec<String>> + Send + 'a>> {
        Box::pin(async move {
            super::ensure_cella_daemon().await;
            super::up::query_daemon_env(container_name, host_gateway).await
        })
    }

    fn register_container<'a>(
        &'a self,
        _client: &'a dyn ContainerBackend,
        container_id: &'a str,
        _config: &'a serde_json::Value,
        _container_name: &'a str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        let id = container_id.to_string();
        Box::pin(async move {
            self.ctx.register_with_daemon(&id).await;
        })
    }

    fn launch_agent<'a>(
        &'a self,
        client: &'a dyn ContainerBackend,
        container_id: &'a str,
        _agent_arch: &'a str,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            launch_agent_exec(client, container_id).await;
        })
    }

    fn post_create_setup<'a>(
        &'a self,
        _client: &'a dyn ContainerBackend,
        container_id: &'a str,
        remote_user: &'a str,
        config: &'a serde_json::Value,
        workspace_root: &'a Path,
        remote_env: &'a [String],
    ) -> Pin<Box<dyn Future<Output = Vec<String>> + Send + 'a>> {
        Box::pin(async move {
            let env_fwd = cella_env::prepare_env_forwarding(config, remote_user, None);
            let settings = cella_config::Settings::load(workspace_root);
            let (_probed_env, lifecycle_env) = self
                .ctx
                .post_create_setup(container_id, remote_user, &env_fwd, &settings, remote_env)
                .await;
            lifecycle_env
        })
    }
}

/// Launch the cella-agent as a background process in the container.
///
/// Since compose's `overrideCommand` defaults to `false`, the container runs its
/// own entrypoint. The agent is started via `exec` as a background daemon.
async fn launch_agent_exec(client: &dyn ContainerBackend, container_id: &str) {
    let agent_path = "/cella/bin/cella-agent";

    let script = format!(
        "if [ -x \"{agent_path}\" ]; then \
         nohup \"{agent_path}\" daemon \
         --poll-interval \"${{CELLA_PORT_POLL_INTERVAL:-1000}}\" \
         > /dev/null 2>&1 & fi"
    );

    debug!("Launching agent in container {container_id}: {agent_path}");
    match client
        .exec_detached(
            container_id,
            &ExecOptions {
                cmd: vec!["sh".to_string(), "-c".to_string(), script],
                user: Some("root".to_string()),
                env: None,
                working_dir: None,
            },
        )
        .await
    {
        Ok(_) => info!("Agent launched in container"),
        Err(e) => warn!("Failed to launch agent in container: {e}"),
    }
}
