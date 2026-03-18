use std::path::PathBuf;

use clap::{Args, ValueEnum};
use serde_json::json;
use tracing::{info, warn};

use cella_config::resolve::resolve_config;
use cella_docker::{
    BuildOptions, ContainerState, DockerClient, container_labels, container_name, image_name,
    lifecycle, update_remote_user_uid,
};

/// Start a dev container for the current workspace.
#[derive(Args)]
pub struct UpArgs {
    /// Rebuild the container image before starting.
    #[arg(long)]
    rebuild: bool,

    /// Explicit Docker host URL (overrides `DOCKER_HOST`).
    #[arg(long)]
    docker_host: Option<String>,

    /// Path to devcontainer.json (overrides auto-discovery).
    #[arg(long)]
    file: Option<PathBuf>,

    /// Output format.
    #[arg(long, value_enum, default_value = "text")]
    output: OutputFormat,
}

/// Output format for container commands.
#[derive(Clone, ValueEnum)]
pub enum OutputFormat {
    Text,
    Json,
}

impl UpArgs {
    #[allow(clippy::too_many_lines)]
    pub async fn execute(self) -> Result<(), Box<dyn std::error::Error>> {
        let cwd = std::env::current_dir()?;

        // 1. Resolve config
        info!("Resolving devcontainer config...");
        let resolved = resolve_config(&cwd)?;

        for w in &resolved.warnings {
            warn!("{}", w.message);
        }

        let config = &resolved.config;
        let config_name = config.get("name").and_then(|v| v.as_str());
        let remote_user = config
            .get("remoteUser")
            .and_then(|v| v.as_str())
            .unwrap_or("root");

        // 2. Run initializeCommand on host (runs every invocation per spec)
        if let Some(init_cmd) = config.get("initializeCommand") {
            run_host_command("initializeCommand", init_cmd)?;
        }

        // 3. Connect to Docker
        let client = match &self.docker_host {
            Some(host) => DockerClient::connect_with_host(host)?,
            None => DockerClient::connect()?,
        };
        client.ping().await?;

        // 4. Check for existing container
        let container_nm = container_name(&resolved.workspace_root, config_name);
        let existing = client.find_container(&resolved.workspace_root).await?;

        let remote_env = map_env_object(config.get("remoteEnv"));
        let workspace_folder = config.get("workspaceFolder").and_then(|v| v.as_str());

        if let Some(container) = existing {
            match (&container.state, self.rebuild) {
                (ContainerState::Running, false) => {
                    // Already running — run postAttachCommand and exit
                    if let Some(cmd) = config.get("postAttachCommand") {
                        lifecycle::run_lifecycle_phase(
                            &client,
                            &container.id,
                            "postAttachCommand",
                            cmd,
                            Some(remote_user),
                            &remote_env,
                            workspace_folder,
                        )
                        .await?;
                    }

                    output_result(
                        &self.output,
                        "existing",
                        &container.id,
                        remote_user,
                        workspace_folder.unwrap_or("/workspaces"),
                    );
                    return Ok(());
                }
                (ContainerState::Running, true) => {
                    info!("Stopping container for rebuild...");
                    client.stop_container(&container.id).await?;
                    client.remove_container(&container.id, false).await?;
                }
                (ContainerState::Stopped, false) => {
                    // Start existing stopped container
                    if let Some(old_hash) = &container.config_hash
                        && *old_hash != resolved.config_hash
                    {
                        warn!(
                            "Config has changed since container was created. Use --rebuild to recreate."
                        );
                    }

                    client.start_container(&container.id).await?;

                    if let Some(cmd) = config.get("postStartCommand") {
                        lifecycle::run_lifecycle_phase(
                            &client,
                            &container.id,
                            "postStartCommand",
                            cmd,
                            Some(remote_user),
                            &remote_env,
                            workspace_folder,
                        )
                        .await?;
                    }

                    if let Some(cmd) = config.get("postAttachCommand") {
                        lifecycle::run_lifecycle_phase(
                            &client,
                            &container.id,
                            "postAttachCommand",
                            cmd,
                            Some(remote_user),
                            &remote_env,
                            workspace_folder,
                        )
                        .await?;
                    }

                    output_result(
                        &self.output,
                        "started",
                        &container.id,
                        remote_user,
                        workspace_folder.unwrap_or("/workspaces"),
                    );
                    return Ok(());
                }
                (_, true) => {
                    // Rebuild: stop if running, then remove
                    if container.state == ContainerState::Running {
                        client.stop_container(&container.id).await?;
                    }
                    client.remove_container(&container.id, false).await?;
                }
                _ => {
                    // Other state (Created, etc.) — remove and recreate
                    let _ = client.remove_container(&container.id, false).await;
                }
            }
        }

        // 5. Ensure image
        let img_name = ensure_image(&client, config, &resolved.workspace_root, config_name).await?;

        // 6. Create container
        let labels = container_labels(
            &resolved.workspace_root,
            &resolved.config_path,
            &resolved.config_hash,
        );

        let create_opts = cella_docker::config_map::map_config(
            config,
            &container_nm,
            &img_name,
            labels,
            &resolved.workspace_root,
        );

        let container_id = client.create_container(&create_opts).await?;

        // 7. Start container
        client.start_container(&container_id).await?;

        // 8. updateRemoteUserUID
        let update_uid = config
            .get("updateRemoteUserUID")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);

        if update_uid
            && remote_user != "root"
            && let Err(e) = update_remote_user_uid(
                &client,
                &container_id,
                remote_user,
                &resolved.workspace_root,
            )
            .await
        {
            warn!("Failed to update remote user UID: {e}");
        }

        // 9-13. Lifecycle commands (first create)
        let lifecycle_phases = [
            "onCreateCommand",
            "updateContentCommand",
            "postCreateCommand",
            "postStartCommand",
            "postAttachCommand",
        ];

        for phase in lifecycle_phases {
            if let Some(cmd) = config.get(phase) {
                lifecycle::run_lifecycle_phase(
                    &client,
                    &container_id,
                    phase,
                    cmd,
                    Some(remote_user),
                    &create_opts.remote_env,
                    workspace_folder,
                )
                .await?;
            }
        }

        // 14. Output
        output_result(
            &self.output,
            "created",
            &container_id,
            remote_user,
            workspace_folder.unwrap_or("/workspaces"),
        );

        Ok(())
    }
}

async fn ensure_image(
    client: &DockerClient,
    config: &serde_json::Value,
    workspace_root: &std::path::Path,
    config_name: Option<&str>,
) -> Result<String, Box<dyn std::error::Error>> {
    if let Some(image) = config.get("image").and_then(|v| v.as_str()) {
        if !client.image_exists(image).await? {
            client.pull_image(image).await?;
        }
        return Ok(image.to_string());
    }

    if let Some(build) = config.get("build").and_then(|v| v.as_object()) {
        let img_name = image_name(workspace_root, config_name);

        let dockerfile = build
            .get("dockerfile")
            .and_then(|v| v.as_str())
            .unwrap_or("Dockerfile")
            .to_string();

        let context = build.get("context").and_then(|v| v.as_str()).unwrap_or(".");

        let context_path = if std::path::Path::new(context).is_absolute() {
            PathBuf::from(context)
        } else {
            workspace_root.join(".devcontainer").join(context)
        };

        let args: std::collections::HashMap<String, String> = build
            .get("args")
            .and_then(|v| v.as_object())
            .map(|obj| {
                obj.iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
                    .collect()
            })
            .unwrap_or_default();

        let target = build
            .get("target")
            .and_then(|v| v.as_str())
            .map(String::from);

        let cache_from: Vec<String> = build
            .get("cacheFrom")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        let build_opts = BuildOptions {
            image_name: img_name.clone(),
            context_path,
            dockerfile,
            args,
            target,
            cache_from,
        };

        client.build_image(&build_opts).await?;
        return Ok(img_name);
    }

    Err("devcontainer.json must specify either 'image' or 'build'".into())
}

fn run_host_command(
    phase: &str,
    value: &serde_json::Value,
) -> Result<(), Box<dyn std::error::Error>> {
    info!("Running {phase} on host");

    match value {
        serde_json::Value::String(s) => {
            run_single_host_command(phase, &["sh", "-c", s])?;
        }
        serde_json::Value::Array(arr) => {
            let cmd: Vec<String> = arr
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect();
            if !cmd.is_empty() {
                let refs: Vec<&str> = cmd.iter().map(String::as_str).collect();
                run_single_host_command(phase, &refs)?;
            }
        }
        serde_json::Value::Object(map) => {
            for (name, v) in map {
                info!("{phase} [{name}]");
                match v {
                    serde_json::Value::String(s) => {
                        run_single_host_command(phase, &["sh", "-c", s])?;
                    }
                    serde_json::Value::Array(arr) => {
                        let cmd: Vec<String> = arr
                            .iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect();
                        if !cmd.is_empty() {
                            let refs: Vec<&str> = cmd.iter().map(String::as_str).collect();
                            run_single_host_command(phase, &refs)?;
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }

    Ok(())
}

fn run_single_host_command(phase: &str, cmd: &[&str]) -> Result<(), Box<dyn std::error::Error>> {
    if cmd.is_empty() {
        return Ok(());
    }

    let status = std::process::Command::new(cmd[0])
        .args(&cmd[1..])
        .status()?;

    if !status.success() {
        return Err(format!(
            "{phase} failed with exit code {}",
            status.code().unwrap_or(-1)
        )
        .into());
    }

    Ok(())
}

fn map_env_object(value: Option<&serde_json::Value>) -> Vec<String> {
    value
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or("")))
                .collect()
        })
        .unwrap_or_default()
}

fn output_result(
    format: &OutputFormat,
    outcome: &str,
    container_id: &str,
    remote_user: &str,
    workspace_folder: &str,
) {
    match format {
        OutputFormat::Text => {
            let short_id = &container_id[..12.min(container_id.len())];
            eprintln!("Container {outcome}. ID: {short_id} Workspace: {workspace_folder}");
        }
        OutputFormat::Json => {
            let output = json!({
                "outcome": outcome,
                "containerId": container_id,
                "remoteUser": remote_user,
                "remoteWorkspaceFolder": workspace_folder,
            });
            println!(
                "{}",
                serde_json::to_string_pretty(&output).unwrap_or_default()
            );
        }
    }
}
