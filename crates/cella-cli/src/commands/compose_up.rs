//! Docker Compose orchestration for `cella up` when `dockerComposeFile` is present.

use std::collections::BTreeMap;

use tracing::{debug, info, warn};

use cella_backend::{
    ContainerBackend, ContainerInfo, ContainerState, ExecOptions, LifecycleContext,
    run_lifecycle_phase,
};
use cella_compose::{ComposeCommand, ComposeProject, OverrideConfig};

use super::up::{
    UpContext, output_result, query_daemon_env, resolve_remote_user, run_all_lifecycle_phases,
    run_host_command, verify_container_running,
};

/// Run the Docker Compose orchestration flow.
///
/// Called from `UpArgs::execute()` when the resolved config contains `dockerComposeFile`.
pub async fn compose_up(ctx: UpContext) -> Result<(), Box<dyn std::error::Error>> {
    let config = ctx.config().clone();

    // 1. Build ComposeProject from resolved config
    let project = ComposeProject::from_resolved(
        &config,
        &ctx.resolved.config_path,
        &ctx.resolved.workspace_root,
    )?;

    info!(
        "Compose project: {} (primary service: {})",
        project.project_name, project.primary_service
    );

    // 2. Validate primary service exists in compose files
    ctx.progress
        .run_step_result("Validating compose configuration...", async {
            cella_compose::parse::validate_primary_service(
                &project.compose_files,
                &project.primary_service,
            )?;
            if let Some(ref run_services) = project.run_services {
                cella_compose::parse::validate_run_services(&project.compose_files, run_services)?;
            }
            Ok::<(), cella_compose::CellaComposeError>(())
        })
        .await?;

    // 3. Run initializeCommand on host (runs every invocation per spec)
    if let Some(init_cmd) = config.get("initializeCommand") {
        run_host_command("initializeCommand", init_cmd)?;
    }

    // 4. Check for existing compose project
    let existing = find_compose_container(
        ctx.client.as_ref(),
        &project.project_name,
        &project.primary_service,
    )
    .await?;

    if let Some(ref container) = existing {
        if let Some(old_hash) = &container.config_hash
            && *old_hash != project.config_hash
            && !ctx.remove_container
        {
            ctx.progress
                .println("  \x1b[33m⚠\x1b[0m Config or compose files changed since last up.");
            ctx.progress
                .println("    Run `cella up --rebuild` to recreate.");
        }

        if container.state == ContainerState::Running
            && !ctx.remove_container
            && !ctx.build_no_cache
        {
            info!("Compose project already running, running postAttachCommand only");
            return handle_compose_running(&ctx, &project, container).await;
        }

        if ctx.remove_container || ctx.build_no_cache {
            ctx.progress
                .run_step_result("Stopping existing compose project...", async {
                    let compose_cmd = ComposeCommand::from_project_name(&project.project_name);
                    compose_cmd.down().await
                })
                .await?;
        }
    }

    // 5-13. Prepare environment, write override, start services
    let (remote_user, resolved_features, agent_arch) =
        prepare_and_start(&ctx, &config, &project).await?;

    // 14-20. Post-start: find container, setup, lifecycle, output
    finalize_compose(
        &ctx,
        &config,
        &project,
        &remote_user,
        resolved_features.as_ref(),
        &agent_arch,
    )
    .await
}

/// Prepare environment, write override YAML, and start compose services (steps 5-13).
async fn prepare_and_start(
    ctx: &UpContext,
    config: &serde_json::Value,
    project: &ComposeProject,
) -> Result<(String, Option<cella_features::ResolvedFeatures>, String), Box<dyn std::error::Error>>
{
    // 5. Check Docker Compose version supports additional_contexts (>= 2.17.0)
    //    before resolving features, so we fail early with a clear message.
    cella_compose::check_compose_features_support().await?;

    // 6. Resolve features via combined-Dockerfile approach (if features configured)
    let features_build = super::compose_features::resolve_compose_features(
        ctx.client.as_ref(),
        config,
        &ctx.resolved.config_path,
        project,
        ctx.build_no_cache,
        &ctx.progress,
    )
    .await?;

    // 6. Ensure daemon is running and get daemon env vars
    super::ensure_cella_daemon().await;
    let daemon_env = query_daemon_env(&ctx.container_nm).await;

    // 7. Detect container architecture and ensure agent volume is populated
    let agent_arch = ctx
        .client
        .detect_container_arch()
        .await
        .unwrap_or_else(|e| {
            warn!("Failed to detect container arch, defaulting to x86_64: {e}");
            "x86_64".to_string()
        });

    let version = env!("CARGO_PKG_VERSION");
    ctx.progress
        .run_step_result("Preparing agent volume...", async {
            ctx.client
                .ensure_agent_provisioned(version, &agent_arch, ctx.skip_checksum)
                .await
        })
        .await?;

    let (agent_vol_name, agent_vol_target, _) = ctx.client.agent_volume_mount();

    // 8. Resolve remote user from config
    let remote_user = resolve_remote_user(config, None, "root");
    let env_fwd = cella_env::prepare_env_forwarding(config, &remote_user, None);

    // 9. Build extra environment variables
    let mut extra_env = daemon_env;
    for e in &env_fwd.env {
        extra_env.push(format!("{}={}", e.key, e.value));
    }
    for e in &ctx.remote_env {
        extra_env.push(e.clone());
    }

    // 10. Build labels for the primary service
    let labels = build_compose_labels(ctx, project, &remote_user);

    // 11. Generate and write override YAML
    let override_config = OverrideConfig {
        primary_service: project.primary_service.clone(),
        image_override: features_build
            .as_ref()
            .and_then(|b| b.image_name_override.clone()),
        override_command: project.override_command,
        agent_volume_name: agent_vol_name,
        agent_volume_target: agent_vol_target,
        extra_env,
        extra_labels: labels,
        build_dockerfile: features_build
            .as_ref()
            .map(|b| b.combined_dockerfile.clone()),
        build_target: features_build.as_ref().map(|b| b.build_target.clone()),
        build_context: features_build
            .as_ref()
            .and_then(|b| b.build_context.clone()),
        additional_contexts: features_build
            .as_ref()
            .map(|b| b.additional_contexts.clone())
            .unwrap_or_default(),
    };
    let override_yaml = cella_compose::override_file::generate_override_yaml(&override_config);
    cella_compose::override_file::write_override_file(&project.override_file, &override_yaml)?;
    debug!(
        "Override file written to: {}",
        project.override_file.display()
    );

    // 12. Run docker compose build (always when features present, or if --build-no-cache)
    let compose_cmd = ComposeCommand::new(project);
    if features_build.is_some() || ctx.build_no_cache {
        ctx.progress
            .run_step_result("Building compose services...", compose_cmd.build(None))
            .await?;
    }

    // 13. docker compose up -d (idempotent)
    ctx.progress
        .run_step_result("Starting compose services...", async {
            compose_cmd.up(project.run_services.as_deref(), false).await
        })
        .await?;

    let resolved_features = features_build.map(|b| b.resolved_features);
    Ok((remote_user, resolved_features, agent_arch))
}

/// Find primary container, run post-create setup, lifecycle phases, and output result (steps 14-20).
async fn finalize_compose(
    ctx: &UpContext,
    config: &serde_json::Value,
    project: &ComposeProject,
    remote_user: &str,
    resolved_features: Option<&cella_features::ResolvedFeatures>,
    agent_arch: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    // 14. Find primary container via compose labels
    let primary = find_compose_container(
        ctx.client.as_ref(),
        &project.project_name,
        &project.primary_service,
    )
    .await?
    .ok_or("Primary service container not found after docker compose up")?;

    info!(
        "Primary container: {} ({})",
        primary.name,
        &primary.id[..12.min(primary.id.len())]
    );

    // 15. Verify primary container is running
    verify_container_running(ctx.client.as_ref(), &primary.id).await?;

    // 16. Register with daemon (primary container only)
    ctx.register_with_daemon(&primary.id).await;

    // 17. Post-create setup (UID, env, credentials, tools, userEnvProbe)
    let env_fwd = cella_env::prepare_env_forwarding(config, remote_user, None);
    let settings = cella_config::Settings::load(&ctx.resolved.workspace_root);
    let (_probed_env, lifecycle_env) = ctx
        .post_create_setup(
            &primary.id,
            remote_user,
            &env_fwd,
            &settings,
            &ctx.remote_env,
        )
        .await;

    // 18. Launch agent as background process via exec
    launch_agent(ctx, &primary.id, agent_arch).await;

    // 19. Run lifecycle phases (primary service only)
    let progress_ref = ctx.progress.clone();
    let lc_ctx = LifecycleContext {
        client: ctx.client.as_ref(),
        container_id: &primary.id,
        user: Some(remote_user),
        env: &lifecycle_env,
        working_dir: Some(project.workspace_folder.as_str()),
        is_text: ctx.progress.is_enabled(),
        on_output: if ctx.progress.is_enabled() {
            Some(Box::new(move |line| progress_ref.println(line)))
        } else {
            None
        },
    };
    run_all_lifecycle_phases(&lc_ctx, config, resolved_features, &ctx.progress).await?;

    // 20. Output result
    output_result(
        &ctx.output,
        "created",
        &primary.id,
        remote_user,
        &project.workspace_folder,
    );

    Ok(())
}

/// Handle a compose project that's already running — just run postAttachCommand.
async fn handle_compose_running(
    ctx: &UpContext,
    project: &ComposeProject,
    container: &ContainerInfo,
) -> Result<(), Box<dyn std::error::Error>> {
    let config = ctx.config();
    let remote_user = resolve_remote_user(config, None, "root");

    // Re-register with daemon in case it restarted
    ctx.register_with_daemon(&container.id).await;

    if let Some(cmd) = config.get("postAttachCommand")
        && !cmd.is_null()
    {
        let lifecycle_env = ctx.remote_env.clone();
        let progress_ref = ctx.progress.clone();
        let lc_ctx = LifecycleContext {
            client: ctx.client.as_ref(),
            container_id: &container.id,
            user: Some(remote_user.as_str()),
            env: &lifecycle_env,
            working_dir: Some(project.workspace_folder.as_str()),
            is_text: ctx.progress.is_enabled(),
            on_output: if ctx.progress.is_enabled() {
                Some(Box::new(move |line| progress_ref.println(line)))
            } else {
                None
            },
        };

        let label = "Running the postAttachCommand from devcontainer.json...";
        ctx.progress.println(&format!("  \x1b[36m▸\x1b[0m {label}"));
        let result =
            run_lifecycle_phase(&lc_ctx, "postAttachCommand", cmd, "devcontainer.json").await;
        match &result {
            Ok(()) => ctx.progress.println(&format!("  \x1b[32m✓\x1b[0m {label}")),
            Err(e) => ctx
                .progress
                .println(&format!("  \x1b[31m✗\x1b[0m {label}: {e}")),
        }
        result?;
    }

    output_result(
        &ctx.output,
        "running",
        &container.id,
        &remote_user,
        &project.workspace_folder,
    );

    Ok(())
}

/// Build cella labels for the compose override file.
fn build_compose_labels(
    ctx: &UpContext,
    project: &ComposeProject,
    remote_user: &str,
) -> BTreeMap<String, String> {
    let mut labels = BTreeMap::new();
    labels.insert("dev.cella.tool".to_string(), "cella".to_string());
    labels.insert(
        "dev.cella.workspace_path".to_string(),
        ctx.resolved
            .workspace_root
            .canonicalize()
            .unwrap_or_else(|_| ctx.resolved.workspace_root.clone())
            .to_string_lossy()
            .to_string(),
    );
    labels.insert(
        "dev.cella.config_path".to_string(),
        ctx.resolved
            .config_path
            .canonicalize()
            .unwrap_or_else(|_| ctx.resolved.config_path.clone())
            .to_string_lossy()
            .to_string(),
    );
    labels.insert(
        "dev.cella.config_hash".to_string(),
        project.config_hash.clone(),
    );
    labels.insert(
        "dev.cella.compose_project".to_string(),
        project.project_name.clone(),
    );
    labels.insert(
        "dev.cella.primary_service".to_string(),
        project.primary_service.clone(),
    );
    labels.insert("dev.cella.remote_user".to_string(), remote_user.to_string());
    labels.insert(
        "dev.cella.workspace_folder".to_string(),
        project.workspace_folder.clone(),
    );
    labels
}

/// Launch the cella-agent as a background process in the container.
///
/// Since compose's `overrideCommand` defaults to `false`, the container runs its
/// own entrypoint. The agent is started via `exec` as a background daemon.
async fn launch_agent(ctx: &UpContext, container_id: &str, _agent_arch: &str) {
    let agent_path = "/cella/bin/cella-agent";

    let script = format!(
        "if [ -x \"{agent_path}\" ]; then \
         nohup \"{agent_path}\" daemon \
         --poll-interval \"${{CELLA_PORT_POLL_INTERVAL:-1000}}\" \
         > /dev/null 2>&1 & fi"
    );

    debug!("Launching agent in container {container_id}: {agent_path}");
    match ctx
        .client
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

/// Find a compose container by project and service name using backend-agnostic
/// label filtering via `list_cella_containers`.
async fn find_compose_container(
    client: &dyn ContainerBackend,
    project_name: &str,
    service_name: &str,
) -> Result<Option<ContainerInfo>, Box<dyn std::error::Error>> {
    let containers = client.list_cella_containers(false).await?;
    let result = containers.into_iter().find(|c| {
        c.labels
            .get("com.docker.compose.project")
            .is_some_and(|p| p == project_name)
            && c.labels
                .get("com.docker.compose.service")
                .is_some_and(|s| s == service_name)
    });
    Ok(result)
}
