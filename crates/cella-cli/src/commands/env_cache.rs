use std::collections::HashMap;

use tracing::debug;

use cella_docker::{DockerClient, ExecOptions};
use cella_env::platform::DockerRuntime;

/// Path inside the container where probed environment is cached.
const PROBED_ENV_CACHE_PATH: &str = "/tmp/.cella-probed-env.json";

/// Read the cached probed environment from a running container.
///
/// Returns `None` if the cache file doesn't exist or can't be parsed
/// (graceful fallback — never errors).
pub async fn read_probed_env_cache(
    client: &DockerClient,
    container_id: &str,
    user: &str,
) -> Option<HashMap<String, String>> {
    let result = client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: vec!["cat".to_string(), PROBED_ENV_CACHE_PATH.to_string()],
                user: Some(user.to_string()),
                env: None,
                working_dir: None,
            },
        )
        .await
        .ok()?;

    if result.exit_code != 0 {
        debug!("No probed env cache found (exit {})", result.exit_code);
        return None;
    }

    let env: HashMap<String, String> = serde_json::from_str(result.stdout.trim()).ok()?;
    debug!("Read {} probed env vars from cache", env.len());
    Some(env)
}

/// Probe the user's environment and cache it inside the container.
///
/// Returns the probed environment, or `None` if probing is disabled or fails.
pub async fn probe_and_cache_user_env(
    client: &DockerClient,
    container_id: &str,
    user: &str,
    probe_type: &str,
) -> Option<HashMap<String, String>> {
    let probe_cmd = cella_env::user_env_probe::probe_command(probe_type, "/bin/sh")?;

    debug!("Running userEnvProbe ({probe_type}): {:?}", probe_cmd);

    let result = client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: probe_cmd,
                user: Some(user.to_string()),
                env: None,
                working_dir: None,
            },
        )
        .await
        .ok()?;

    if result.exit_code != 0 {
        debug!(
            "userEnvProbe failed (exit {}): {}",
            result.exit_code,
            result.stderr.trim()
        );
        return None;
    }

    let env = cella_env::user_env_probe::parse_probed_env(&result.stdout);
    if env.is_empty() {
        return None;
    }

    // Cache the result inside the container
    let json = serde_json::to_string(&env).ok()?;
    let cache_file = cella_docker::FileToUpload {
        path: PROBED_ENV_CACHE_PATH.to_string(),
        content: json.into_bytes(),
        mode: 0o644,
    };

    if let Err(e) = client.upload_files(container_id, &[cache_file]).await {
        debug!("Failed to cache probed env: {e}");
    }

    debug!("Cached {} probed env vars", env.len());
    Some(env)
}

/// If `SSH_AUTH_SOCK` is not already present in `env`, detect the runtime and
/// check whether the well-known socket path exists inside the container.
/// Appends `SSH_AUTH_SOCK=<path>` if found.
pub async fn ensure_ssh_auth_sock(
    client: &DockerClient,
    container_id: &str,
    user: &str,
    env: &mut Vec<String>,
) {
    if env.iter().any(|e| e.starts_with("SSH_AUTH_SOCK=")) {
        return;
    }

    let runtime = cella_env::platform::detect_runtime();
    let socket_path = if matches!(runtime, DockerRuntime::DockerDesktop) {
        "/run/host-services/ssh-auth.sock"
    } else {
        "/tmp/cella-ssh-agent.sock"
    };

    let result = client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: vec![
                    "test".to_string(),
                    "-e".to_string(),
                    socket_path.to_string(),
                ],
                user: Some(user.to_string()),
                env: None,
                working_dir: None,
            },
        )
        .await;

    if let Ok(r) = result
        && r.exit_code == 0
    {
        debug!("SSH_AUTH_SOCK fallback: found {socket_path}");
        env.push(format!("SSH_AUTH_SOCK={socket_path}"));
    }
}
