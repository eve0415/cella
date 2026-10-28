use std::collections::HashMap;
use std::time::Duration;

use tracing::{debug, warn};

use cella_backend::{ContainerBackend, ContainerInfo, ExecOptions, FileToUpload};
use cella_config::devcontainer::subst::{CONTAINER_ENV_TOKEN, SubstitutionContext};
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
    let mut env = run_env_probe(client, container_id, user, probe_type, shell).await?;
    merge_container_base_path(client, container_id, user, &mut env).await;
    write_env_cache(client, container_id, user, probe_type, &env).await;
    Some(env)
}

/// Try one probe method and return the parsed env, or an error to signal the
/// caller should fall back to the next method.
///
/// Returns `Ok(env)` on success. Returns `Err(true)` on timeout (caller must
/// not retry — a hang is a hang). Returns `Err(false)` on exec error,
/// non-zero exit, or when the parsed map is empty.
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

/// Whether `user` identifies the root account.
///
/// Docker's `--user` flag accepts several formats: a plain name (`root`), a
/// plain UID (`0`), or `uid:gid` / `name:group` pairs (`0:0`, `root:root`,
/// `root:0`, `0:root`). Inspect the part before `:` (if any) and treat the
/// user as root when it is `"root"` or `"0"`.
fn is_root_user(user: &str) -> bool {
    let uid_or_name = user.split(':').next().unwrap_or(user);
    uid_or_name == "root" || uid_or_name == "0"
}

/// Merge the container's base `PATH` (from image config) into the probed env so
/// PATH entries a shell startup script dropped are recovered. Non-fatal: any
/// lookup failure leaves the probed PATH untouched. Mirrors the official CLI.
async fn merge_container_base_path(
    client: &dyn ContainerBackend,
    container_id: &str,
    user: &str,
    env: &mut HashMap<String, String>,
) {
    let Some(shell_path) = env.get("PATH").cloned() else {
        return;
    };
    let Ok(info) = client.inspect_container(container_id).await else {
        return;
    };
    let Some(image) = info.image else {
        return;
    };
    let Ok(image_env) = client.inspect_image_env(&image).await else {
        return;
    };
    let Some(container_path) = image_env.iter().find_map(|e| e.strip_prefix("PATH=")) else {
        return;
    };
    let root_user = is_root_user(user);
    let merged = cella_env::user_env_probe::merge_paths(&shell_path, container_path, root_user);
    env.insert("PATH".to_string(), merged);
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
                cmd: vec!["mkdir".to_string(), "-p".to_string(), dir_path.clone()],
                user: Some(user.to_string()),
                env: None,
                working_dir: None,
            },
        )
        .await;

    let cache_file = FileToUpload {
        path: path.clone(),
        content: json.into_bytes(),
        mode: 0o644,
    };

    if let Err(e) = client.upload_files(container_id, &[cache_file]).await {
        debug!("Failed to cache probed env: {e}");
        return;
    }
    debug!("Cached {} probed env vars", env.len());

    // Tar extraction runs as root and rewrites the directory entry it creates,
    // so the `mkdir` above does not settle ownership. Nothing else repairs it.
    cella_backend::container_setup::chown_path_in_container(client, container_id, user, &dir_path)
        .await;
    cella_backend::container_setup::chown_path_in_container(client, container_id, user, &path)
        .await;
}

/// Ensure `SSH_AUTH_SOCK` is present when a well-known runtime-specific socket exists inside the container.
///
/// No-op for backends whose runtime forwards the agent because it already sets `SSH_AUTH_SOCK` for exec processes.
pub async fn ensure_ssh_auth_sock(
    client: &dyn ContainerBackend,
    container_id: &str,
    user: &str,
    env: &mut Vec<String>,
) {
    match client.capabilities().ssh_agent {
        cella_backend::SshAgentTransport::BindMount => {}
        cella_backend::SshAgentTransport::Runtime => return,
    }

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

/// Read the config `remoteEnv` entries stored in the `dev.cella.remote_env`
/// label, finishing any `${containerEnv:...}` against the container's own
/// configured env.
///
/// Mirrors the official CLI, which re-substitutes `${containerEnv:...}` at
/// exec time against the live container. The container is only inspected
/// when some entry still carries the token.
pub async fn label_remote_env(
    client: &dyn ContainerBackend,
    container: &ContainerInfo,
) -> Vec<String> {
    let entries: Vec<String> = container
        .labels
        .get("dev.cella.remote_env")
        .and_then(|v| serde_json::from_str(v).ok())
        .unwrap_or_default();
    if !entries.iter().any(|e| e.contains(CONTAINER_ENV_TOKEN)) {
        return entries;
    }
    let container_env = client
        .inspect_container_env(&container.id)
        .await
        .unwrap_or_else(|e| {
            warn!("Could not read container env to resolve remoteEnv: {e}");
            Vec::new()
        });
    expand_container_env(entries, &container_env)
}

/// Resolve `${containerEnv:...}` in the values of `KEY=value` entries against
/// `container_env` (also `KEY=value`). Entries without the token pass through.
fn expand_container_env(entries: Vec<String>, container_env: &[String]) -> Vec<String> {
    let env: HashMap<String, String> = container_env
        .iter()
        .filter_map(|e| e.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let ctx = SubstitutionContext::container_env_only(env);
    entries
        .into_iter()
        .map(|entry| match entry.split_once('=') {
            Some((key, value)) if value.contains(CONTAINER_ENV_TOKEN) => {
                format!("{key}={}", ctx.substitute_str(value))
            }
            _ => entry,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn container_with_remote_env(entries: &[&str]) -> ContainerInfo {
        ContainerInfo {
            id: "c1".to_string(),
            name: "c1".to_string(),
            state: cella_backend::ContainerState::Running,
            exit_code: None,
            labels: HashMap::from([(
                "dev.cella.remote_env".to_string(),
                serde_json::to_string(entries).expect("serialize"),
            )]),
            config_hash: None,
            ports: Vec::new(),
            created_at: None,
            started_at: None,
            container_user: None,
            image: None,
            mounts: Vec::new(),
            backend: cella_backend::BackendKind::Docker,
        }
    }

    /// Regression: the label keeps `${containerEnv:PATH}` and it must resolve
    /// against the container's own PATH, not be injected as literal text.
    #[test]
    fn expand_container_env_resolves_against_container_env() {
        let entries = vec![concat!("PATH=${containerEnv", ":PATH}:/x").to_string()];
        let container_env = ["PATH=/usr/bin:/bin".to_string()];
        assert_eq!(
            expand_container_env(entries, &container_env),
            vec!["PATH=/usr/bin:/bin:/x"]
        );
    }

    /// Everything but `${containerEnv:...}` was expanded at `up`; whatever is
    /// left in the value (e.g. text that came from a host env var) stays put.
    #[test]
    fn expand_container_env_touches_only_container_env() {
        let entries =
            vec![concat!("MIX=${localEnv", ":HOME}|${containerEnv", ":PATH}").to_string()];
        let container_env = ["PATH=/usr/bin".to_string(), "HOME=/root".to_string()];
        assert_eq!(
            expand_container_env(entries, &container_env),
            vec![concat!("MIX=${localEnv", ":HOME}|/usr/bin")]
        );
    }

    #[test]
    fn expand_container_env_uses_default_when_missing() {
        let entries = vec![concat!("TOOL=${containerEnv", ":MISSING:fallback}").to_string()];
        assert_eq!(
            expand_container_env(entries, &["PATH=/usr/bin".to_string()]),
            vec!["TOOL=fallback"]
        );
    }

    #[test]
    fn expand_container_env_passes_other_entries_through() {
        let entries = vec![
            "EDITOR=vim".to_string(),
            concat!("RAW=${localEnv", ":HOME}").to_string(),
            "EMPTY=".to_string(),
        ];
        assert_eq!(
            expand_container_env(entries.clone(), &["HOME=/root".to_string()]),
            entries
        );
    }

    #[tokio::test]
    async fn label_remote_env_inspects_container_only_when_needed() {
        let (backend, recorded) =
            crate::run_user_commands::tests::LifecycleMockBackend::new(true, "unused");
        let backend = backend.with_container_env(vec!["PATH=/usr/bin:/bin".to_string()]);

        let plain = container_with_remote_env(&["EDITOR=vim"]);
        assert_eq!(label_remote_env(&backend, &plain).await, vec!["EDITOR=vim"]);
        assert!(recorded.lock().expect("mutex").is_empty());

        let deferred =
            container_with_remote_env(&[concat!("PATH=${containerEnv", ":PATH}:/x"), "EDITOR=vim"]);
        assert_eq!(
            label_remote_env(&backend, &deferred).await,
            vec!["PATH=/usr/bin:/bin:/x", "EDITOR=vim"]
        );
        assert_eq!(
            *recorded.lock().expect("mutex"),
            vec!["inspect_container_env c1"]
        );
    }

    #[tokio::test]
    async fn runtime_ssh_agent_skips_socket_probe() {
        let (backend, recorded) =
            crate::run_user_commands::tests::LifecycleMockBackend::new(true, "unused");
        let backend = backend.with_ssh_agent_transport(cella_backend::SshAgentTransport::Runtime);
        let mut env = vec!["EXAMPLE=value".to_string()];

        ensure_ssh_auth_sock(&backend, "container", "user", &mut env).await;

        assert_eq!(env, vec!["EXAMPLE=value"]);
        assert!(recorded.lock().unwrap().is_empty());
    }

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

    // -- is_root_user tests --

    /// Regression: Docker `--user` accepts `uid:gid` and `name:group` forms.
    /// `is_root_user` must recognise all root-identifying formats so that sbin
    /// entries are correctly retained in the merged PATH for root containers.
    #[test]
    fn is_root_user_plain_name() {
        assert!(is_root_user("root"));
    }

    #[test]
    fn is_root_user_plain_uid() {
        assert!(is_root_user("0"));
    }

    #[test]
    fn is_root_user_uid_gid() {
        assert!(is_root_user("0:0"));
    }

    #[test]
    fn is_root_user_name_group() {
        assert!(is_root_user("root:root"));
    }

    #[test]
    fn is_root_user_root_gid() {
        assert!(is_root_user("root:0"));
    }

    #[test]
    fn is_root_user_uid_group_name() {
        assert!(is_root_user("0:root"));
    }

    #[test]
    fn is_root_user_non_root() {
        assert!(!is_root_user("vscode"));
        assert!(!is_root_user("1000"));
        assert!(!is_root_user("1000:1000"));
        assert!(!is_root_user("user:group"));
    }
}
