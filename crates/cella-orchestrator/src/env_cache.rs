use std::collections::HashMap;
use std::time::Duration;

use tracing::{debug, warn};

use cella_backend::{ContainerBackend, ExecOptions, FileToUpload};
use cella_env::platform::DockerRuntime;
use cella_env::user_env_probe::{
    PROBE_METHODS, ProbeMethod, UserEnvProbe, parse_probed_env, probe_command,
};

/// Compute the per-user, per-probe-type cache path for the probed environment.
///
/// Stores under `$HOME/.cella/env-{probeType}.json` so different probe types
/// don't serve stale results from a previous probe configuration.
fn cache_path(user: &str, probe_type: UserEnvProbe) -> String {
    let home = cella_env::claude_code::container_home(user);
    format!("{home}/.cella/env-{probe_type}.json")
}

/// Read the cached probed environment from a running container.
///
/// Returns `None` if the cache file doesn't exist or can't be parsed
/// (graceful fallback, never errors).
pub async fn read_probed_env_cache(
    client: &dyn ContainerBackend,
    container_id: &str,
    user: &str,
    probe_type: UserEnvProbe,
) -> Option<HashMap<String, String>> {
    if probe_type == UserEnvProbe::None {
        return None;
    }
    let path = cache_path(user, probe_type);
    let result = client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: vec!["cat".to_string(), path],
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
    client: &dyn ContainerBackend,
    container_id: &str,
    user: &str,
    probe_type: UserEnvProbe,
    shell: &str,
) -> Option<HashMap<String, String>> {
    let env = run_env_probe(client, container_id, user, probe_type, shell).await?;
    write_env_cache(client, container_id, user, probe_type, &env).await;
    Some(env)
}

/// Try one probe method and return the parsed env, or `None` to signal the
/// caller should fall back to the next method.
///
/// Returns `None` on timeout (caller must not retry — a hang is a hang), on
/// exec error, on non-zero exit, or when the parsed map is empty.
/// A timeout is signalled by setting the bool to `true`; all other failures
/// leave it `false`.
async fn try_probe_method(
    client: &dyn ContainerBackend,
    container_id: &str,
    user: &str,
    probe_type: UserEnvProbe,
    shell: &str,
    marker: &str,
    method: ProbeMethod,
) -> Result<HashMap<String, String>, bool> {
    let Some(cmd) = probe_command(probe_type, shell, marker, method) else {
        // Only happens for UserEnvProbe::None — already guarded in run_env_probe.
        return Err(false);
    };

    debug!(
        "Running userEnvProbe ({probe_type}) with {shell} via `{}`: {cmd:?}",
        method.command
    );

    let exec_opts = ExecOptions {
        cmd,
        user: Some(user.to_string()),
        env: None,
        working_dir: None,
    };
    let exec_future = client.exec_command(container_id, &exec_opts);

    let result = match tokio::time::timeout(Duration::from_secs(10), exec_future).await {
        Ok(Ok(r)) => r,
        Ok(Err(_)) => return Err(false),
        Err(_) => {
            warn!(
                "userEnvProbe timed out after 10s \
                 — avoid waiting for user input in shell startup scripts"
            );
            return Err(true); // timed out — abort all further methods
        }
    };

    if result.exit_code != 0 {
        debug!(
            "userEnvProbe method `{}` failed (exit {}): {}",
            method.command,
            result.exit_code,
            result.stderr.trim()
        );
        return Err(false);
    }

    let env = parse_probed_env(&result.stdout, marker, method.separator);
    if env.is_empty() {
        debug!(
            "userEnvProbe method `{}` yielded empty env — trying fallback",
            method.command
        );
        return Err(false);
    }

    Ok(env)
}

/// Execute the environment probe command and parse the output.
///
/// Tries each method in [`PROBE_METHODS`] in order, returning the first
/// non-empty result. A UUID marker is embedded in the shell command so that
/// shell-startup noise printed to stdout is stripped before parsing.
///
/// Applies a 10-second timeout per method. A timeout aborts all further
/// attempts — a hanging shell startup script won't be retried.
async fn run_env_probe(
    client: &dyn ContainerBackend,
    container_id: &str,
    user: &str,
    probe_type: UserEnvProbe,
    shell: &str,
) -> Option<HashMap<String, String>> {
    if probe_type == UserEnvProbe::None {
        return None;
    }

    let marker = uuid::Uuid::new_v4().to_string();

    for method in PROBE_METHODS {
        match try_probe_method(
            client,
            container_id,
            user,
            probe_type,
            shell,
            &marker,
            *method,
        )
        .await
        {
            Ok(env) => return Some(env),
            Err(true) => return None, // timed out — don't retry a hanging shell
            Err(false) => {}          // other failure — fall back to next method
        }
    }

    None
}

/// Write the probed environment to a cache file inside the container.
///
/// Creates the `~/.cella/` directory if it doesn't exist.
async fn write_env_cache(
    client: &dyn ContainerBackend,
    container_id: &str,
    user: &str,
    probe_type: UserEnvProbe,
    env: &HashMap<String, String>,
) {
    let Some(json) = serde_json::to_string(env).ok() else {
        return;
    };

    let path = cache_path(user, probe_type);

    let home = cella_env::claude_code::container_home(user);
    let dir_path = format!("{home}/.cella");
    let _ = client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: vec!["mkdir".to_string(), "-p".to_string(), dir_path],
                user: Some(user.to_string()),
                env: None,
                working_dir: None,
            },
        )
        .await;

    let cache_file = FileToUpload {
        path,
        content: json.into_bytes(),
        mode: 0o644,
    };

    if let Err(e) = client.upload_files(container_id, &[cache_file]).await {
        debug!("Failed to cache probed env: {e}");
    } else {
        debug!("Cached {} probed env vars", env.len());
    }
}

/// Ensure `SSH_AUTH_SOCK` is present in the target environment when a
/// well-known runtime-specific socket exists inside the container.
pub async fn ensure_ssh_auth_sock(
    client: &dyn ContainerBackend,
    container_id: &str,
    user: &str,
    env: &mut Vec<String>,
) {
    if env.iter().any(|e| e.starts_with("SSH_AUTH_SOCK=")) {
        return;
    }

    let runtime = cella_env::platform::detect_runtime();
    let socket_path = if matches!(
        runtime,
        DockerRuntime::DockerDesktop | DockerRuntime::OrbStack
    ) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_path_includes_probe_type() {
        let path = cache_path("vscode", UserEnvProbe::LoginInteractiveShell);
        assert!(path.contains("vscode"));
        assert!(path.ends_with("/.cella/env-loginInteractiveShell.json"));
    }

    #[test]
    fn cache_path_none_probe() {
        let path = cache_path("vscode", UserEnvProbe::None);
        assert!(path.ends_with("/.cella/env-none.json"));
    }

    #[test]
    fn cache_path_root_user() {
        let path = cache_path("root", UserEnvProbe::LoginShell);
        assert!(path.contains("root"));
        assert!(path.ends_with("/.cella/env-loginShell.json"));
    }

    #[test]
    fn cache_path_interactive_shell() {
        let path = cache_path("devuser", UserEnvProbe::InteractiveShell);
        assert!(path.ends_with("/.cella/env-interactiveShell.json"));
    }
}
