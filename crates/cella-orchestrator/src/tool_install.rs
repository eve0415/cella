//! Tool installation helpers for AI coding tools (Claude Code, Codex, Gemini).
//!
//! These functions install and configure AI coding tools inside dev containers.
//! They were extracted from the CLI `up` command to be reusable by both the CLI
//! and daemon.

use std::collections::HashMap;

use cella_docker::{CellaDockerError, DockerClient, ExecOptions, ExecResult, MountConfig};
use tracing::{debug, warn};

use crate::progress::ProgressSender;

/// Probed user environment (e.g. from `userEnvProbe`).
///
/// Concrete type alias avoids generic hasher parameters on every helper function.
type ProbedEnv = HashMap<String, String>;

// ── Tool exec helpers ────────────────────────────────────────────────────────

/// Extract PATH from the probed user environment for tool exec calls.
///
/// Returns `Some(vec!["PATH=..."])` when the probed env contains PATH,
/// `None` otherwise (caller should fall back to a login shell).
pub fn tool_exec_env(probed_env: Option<&ProbedEnv>) -> Option<Vec<String>> {
    probed_env
        .and_then(|env| env.get("PATH"))
        .map(|path| vec![format!("PATH={path}")])
}

/// Build the shell command prefix for a tool exec call.
///
/// When the probed env is available (and thus PATH will be passed via `env`),
/// uses a plain `sh -c`. Otherwise falls back to a login shell (`sh -l -c`)
/// so that `/etc/profile.d/` scripts (e.g. nvm) are sourced.
pub fn tool_shell_cmd(probed_env: Option<&ProbedEnv>, inner_cmd: &str) -> Vec<String> {
    if probed_env.and_then(|e| e.get("PATH")).is_some() {
        vec!["sh".to_string(), "-c".to_string(), inner_cmd.to_string()]
    } else {
        vec![
            "sh".to_string(),
            "-l".to_string(),
            "-c".to_string(),
            inner_cmd.to_string(),
        ]
    }
}

// ── Alpine detection ─────────────────────────────────────────────────────────

/// Check if the container is Alpine-based.
pub async fn is_alpine_container(client: &DockerClient, container_id: &str) -> bool {
    client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: vec![
                    "test".to_string(),
                    "-f".to_string(),
                    "/etc/alpine-release".to_string(),
                ],
                user: Some("root".to_string()),
                env: None,
                working_dir: None,
            },
        )
        .await
        .is_ok_and(|r| r.exit_code == 0)
}

// ── Node.js / npm ────────────────────────────────────────────────────────────

/// Ensure Node.js and npm are available in the container.
///
/// Uses the probed user environment PATH (from `userEnvProbe`) to detect
/// npm installed by devcontainer features (e.g. nvm). Falls back to a login
/// shell when no probed env is available. If npm is still not found, attempts
/// to install Node.js via the system package manager (apt-get or apk).
/// Returns `true` if npm is available after the check.
pub async fn ensure_node_available(
    client: &DockerClient,
    container_id: &str,
    probed_env: Option<&ProbedEnv>,
) -> bool {
    let npm_check = client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: tool_shell_cmd(probed_env, "command -v npm"),
                user: Some("root".to_string()),
                env: tool_exec_env(probed_env),
                working_dir: None,
            },
        )
        .await;

    if npm_check.is_ok_and(|r| r.exit_code == 0) {
        return true;
    }

    debug!("npm not found, installing Node.js...");
    let install_cmd = if is_alpine_container(client, container_id).await {
        "apk add --no-cache nodejs npm"
    } else {
        "apt-get update -qq && apt-get install -y -qq nodejs npm"
    };

    let result = client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: vec!["sh".to_string(), "-c".to_string(), install_cmd.to_string()],
                user: Some("root".to_string()),
                env: None,
                working_dir: None,
            },
        )
        .await;

    match &result {
        Ok(r) if r.exit_code == 0 => {
            debug!("Node.js installed successfully");
            true
        }
        Ok(r) => {
            warn!(
                "Node.js installation failed (exit {}): {}",
                r.exit_code,
                r.stderr.trim()
            );
            false
        }
        Err(e) => {
            warn!("Node.js installation failed: {e}");
            false
        }
    }
}

// ── Claude Code ──────────────────────────────────────────────────────────────

/// Check if Claude Code is already installed at the desired version.
/// Returns `true` if already installed and no (re)install is needed.
pub async fn is_claude_code_installed(
    client: &DockerClient,
    container_id: &str,
    remote_user: &str,
    version: &str,
    probed_env: Option<&ProbedEnv>,
) -> bool {
    let version_check = client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: tool_shell_cmd(probed_env, "claude --version"),
                user: Some(remote_user.to_string()),
                env: tool_exec_env(probed_env),
                working_dir: None,
            },
        )
        .await;

    if let Ok(result) = &version_check
        && result.exit_code == 0
    {
        let installed = result.stdout.trim();
        if version == "latest" || version == "stable" {
            debug!("Claude Code already installed: {installed}");
            return true;
        }
        if installed.contains(version) {
            debug!("Claude Code already at version {version}: {installed}");
            return true;
        }
    }
    false
}

/// Detect Alpine and install Claude Code native dependencies if needed.
/// Returns `true` if the container is Alpine-based.
pub async fn ensure_alpine_claude_deps(client: &DockerClient, container_id: &str) -> bool {
    let is_alpine = is_alpine_container(client, container_id).await;

    if is_alpine {
        debug!("Alpine detected, installing Claude Code dependencies...");
        let _ = client
            .exec_command(
                container_id,
                &ExecOptions {
                    cmd: vec![
                        "sh".to_string(),
                        "-c".to_string(),
                        "apk add --no-cache libgcc libstdc++ ripgrep".to_string(),
                    ],
                    user: Some("root".to_string()),
                    env: None,
                    working_dir: None,
                },
            )
            .await;
    }
    is_alpine
}

/// Install Claude Code inside the container.
///
/// Checks if already installed at the desired version, installs Alpine
/// dependencies if needed, then runs the native installer.
pub async fn install_claude_code(
    client: &DockerClient,
    container_id: &str,
    remote_user: &str,
    settings: &cella_config::ClaudeCode,
    probed_env: Option<&ProbedEnv>,
) {
    if is_claude_code_installed(
        client,
        container_id,
        remote_user,
        &settings.version,
        probed_env,
    )
    .await
    {
        return;
    }

    let is_alpine = ensure_alpine_claude_deps(client, container_id).await;
    run_claude_install(
        client,
        container_id,
        remote_user,
        &settings.version,
        is_alpine,
        probed_env,
    )
    .await;
}

/// Execute the Claude Code install script inside the container.
pub async fn run_claude_install(
    client: &DockerClient,
    container_id: &str,
    remote_user: &str,
    version: &str,
    is_alpine: bool,
    probed_env: Option<&ProbedEnv>,
) {
    if version != "latest" && version != "stable" {
        debug!("Installing Claude Code v{version} (native installer will attempt version pinning)");
    }

    let install_cmd = format!("curl -fsSL https://claude.ai/install.sh | bash -s {version}");
    debug!("Installing Claude Code ({version})...");

    let mut env = tool_exec_env(probed_env).unwrap_or_default();
    if is_alpine {
        env.push("USE_BUILTIN_RIPGREP=0".to_string());
    }
    let env = if env.is_empty() { None } else { Some(env) };

    let result = client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: vec!["sh".to_string(), "-c".to_string(), install_cmd],
                user: Some(remote_user.to_string()),
                env,
                working_dir: None,
            },
        )
        .await;

    log_install_result(result);
}

/// Log the result of a Claude Code installation attempt.
pub fn log_install_result(result: Result<ExecResult, CellaDockerError>) {
    match result {
        Ok(r) if r.exit_code == 0 => {
            debug!("Claude Code installed successfully");
        }
        Ok(r) => {
            warn!(
                "Claude Code installation exited with code {}: {}",
                r.exit_code,
                r.stderr.trim()
            );
        }
        Err(e) => {
            warn!("Claude Code installation failed: {e}");
        }
    }
}

// ── npm tool helpers ─────────────────────────────────────────────────────────

/// Check if an npm-installed CLI tool is already present at the desired version.
pub async fn is_npm_tool_installed(
    client: &DockerClient,
    container_id: &str,
    remote_user: &str,
    binary_name: &str,
    version: &str,
    probed_env: Option<&ProbedEnv>,
) -> bool {
    let version_check = client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: tool_shell_cmd(probed_env, &format!("{binary_name} --version")),
                user: Some(remote_user.to_string()),
                env: tool_exec_env(probed_env),
                working_dir: None,
            },
        )
        .await;

    if let Ok(result) = &version_check
        && result.exit_code == 0
    {
        let installed = result.stdout.trim();
        if version == "latest" {
            debug!("{binary_name} already installed: {installed}");
            return true;
        }
        if installed.contains(version) {
            debug!("{binary_name} already at version {version}: {installed}");
            return true;
        }
    }
    false
}

/// Install an npm package globally inside the container.
///
/// # Errors
///
/// Returns `CellaDockerError` if the exec command fails to run.
pub async fn npm_install_global(
    client: &DockerClient,
    container_id: &str,
    remote_user: &str,
    package: &str,
    version: &str,
    probed_env: Option<&ProbedEnv>,
) -> Result<ExecResult, CellaDockerError> {
    let pkg = if version == "latest" {
        package.to_string()
    } else {
        format!("{package}@{version}")
    };

    client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: tool_shell_cmd(probed_env, &format!("npm install -g {pkg}")),
                user: Some(remote_user.to_string()),
                env: tool_exec_env(probed_env),
                working_dir: None,
            },
        )
        .await
}

/// Log the result of an npm tool installation attempt.
pub fn log_npm_install_result(tool_name: &str, result: Result<ExecResult, CellaDockerError>) {
    match result {
        Ok(r) if r.exit_code == 0 => {
            debug!("{tool_name} installed successfully");
        }
        Ok(r) => {
            warn!(
                "{tool_name} installation exited with code {}: {}",
                r.exit_code,
                r.stderr.trim()
            );
        }
        Err(e) => {
            warn!("{tool_name} installation failed: {e}");
        }
    }
}

// ── Codex ────────────────────────────────────────────────────────────────────

/// Install `OpenAI` Codex CLI inside the container via npm.
///
/// Checks if already installed, then runs `npm install -g @openai/codex`.
/// Caller must ensure Node.js/npm are available before calling this.
pub async fn install_codex(
    client: &DockerClient,
    container_id: &str,
    remote_user: &str,
    settings: &cella_config::Codex,
    probed_env: Option<&ProbedEnv>,
) {
    if is_npm_tool_installed(
        client,
        container_id,
        remote_user,
        "codex",
        &settings.version,
        probed_env,
    )
    .await
    {
        return;
    }

    debug!("Installing Codex ({})...", settings.version);
    let result = npm_install_global(
        client,
        container_id,
        remote_user,
        "@openai/codex",
        &settings.version,
        probed_env,
    )
    .await;
    log_npm_install_result("Codex", result);
}

// ── Gemini ───────────────────────────────────────────────────────────────────

/// Install Google Gemini CLI inside the container via npm.
///
/// Checks if already installed, then runs `npm install -g @google/gemini-cli`.
/// Caller must ensure Node.js/npm are available before calling this.
pub async fn install_gemini(
    client: &DockerClient,
    container_id: &str,
    remote_user: &str,
    settings: &cella_config::Gemini,
    probed_env: Option<&ProbedEnv>,
) {
    if is_npm_tool_installed(
        client,
        container_id,
        remote_user,
        "gemini",
        &settings.version,
        probed_env,
    )
    .await
    {
        return;
    }

    debug!("Installing Gemini CLI ({})...", settings.version);
    let result = npm_install_global(
        client,
        container_id,
        remote_user,
        "@google/gemini-cli",
        &settings.version,
        probed_env,
    )
    .await;
    log_npm_install_result("Gemini CLI", result);
}

// ── Claude Code config helpers ───────────────────────────────────────────────

/// Create a symlink from the host's `.claude` path to the container's so that
/// hardcoded paths in plugin manifests (`installed_plugins.json`, `known_marketplaces.json`)
/// resolve transparently.
///
/// Example: host home `/home/node`, container home `/home/vscode`:
///   `/home/node/.claude` -> `/home/vscode/.claude`
pub async fn create_claude_home_symlink(
    client: &DockerClient,
    container_id: &str,
    remote_user: &str,
) {
    let Some(host_home) = cella_env::claude_code::host_home() else {
        return;
    };
    let container_home = cella_env::claude_code::container_home(remote_user);

    let host_home_str = host_home.to_string_lossy();
    if *host_home_str == container_home {
        return;
    }

    let claude_target = format!("{container_home}/.claude");
    let claude_link = format!("{host_home_str}/.claude");
    let cmd = format!("mkdir -p {host_home_str} && ln -sfn {claude_target} {claude_link}");

    let _ = client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: vec!["sh".into(), "-c".into(), cmd],
                user: Some("root".into()),
                env: None,
                working_dir: None,
            },
        )
        .await;
}

/// Populate the tmpfs-backed `~/.claude/plugins/` directory.
///
/// Creates symlinks for plugin content (cache/, data/, marketplaces/) pointing
/// to the hidden host mount at `/tmp/.cella/host-plugins/`, and copies
/// `installed_plugins.json` and `known_marketplaces.json` with path rewriting.
///
/// Uses regex-based sed to match ANY home path + `/.claude` (Linux, macOS, root)
/// and replace with the container user's path. This handles files written by
/// previous containers with different users (e.g. `/home/node/.claude` ->
/// `/home/vscode/.claude`).
pub async fn setup_plugin_manifests(client: &DockerClient, container_id: &str, remote_user: &str) {
    let container_home = cella_env::claude_code::container_home(remote_user);
    let plugins_dir = format!("{container_home}/.claude/plugins");
    let host_plugins = "/tmp/.cella/host-plugins";
    let target_claude = format!("{container_home}/.claude");

    // Regex sed: rewrite /home/USER/.claude, /Users/USER/.claude, /root/.claude
    // to the container user's path. Handles any previous writer.
    let sed_expr = format!(
        concat!(
            "s|/home/[^/\"]*/.claude|{t}|g; ",
            "s|/Users/[^/\"]*/.claude|{t}|g; ",
            "s|/root/.claude|{t}|g",
        ),
        t = target_claude,
    );

    // Symlink all items except the 2 manifest JSONs (which get copied + rewritten)
    let script = format!(
        concat!(
            "[ -d \"{host}\" ] || exit 0; ",
            "for item in \"{host}\"/* \"{host}\"/.*; do ",
            "  [ -e \"$item\" ] || continue; ",
            "  name=$(basename \"$item\"); ",
            "  case \"$name\" in ",
            "    .|..) continue ;; ",
            "    installed_plugins.json|known_marketplaces.json) ",
            "      [ -f \"$item\" ] && sed -E '{sed}' \"$item\" > \"{dir}/$name\" ;; ",
            "    *) ln -sfn \"$item\" \"{dir}/$name\" ;; ",
            "  esac; ",
            "done",
        ),
        host = host_plugins,
        dir = plugins_dir,
        sed = sed_expr,
    );

    let _ = client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: vec!["sh".into(), "-c".into(), script],
                user: Some("root".into()),
                env: None,
                working_dir: None,
            },
        )
        .await;

    crate::container_setup::chown_in_container(client, container_id, remote_user, &plugins_dir)
        .await;
}

// ── Tool config mounts ───────────────────────────────────────────────────────

/// Add bind mounts for tool config directories (Claude Code, Codex, Gemini, nvim, tmux).
pub fn add_tool_config_mounts(
    create_opts: &mut cella_docker::CreateContainerOptions,
    settings: &cella_config::Settings,
    remote_user: &str,
) {
    // Claude Code: ~/.claude.json (single file) and ~/.claude/ (directory)
    if settings.tools.claude_code.forward_config {
        if let Some(host_path) = cella_env::claude_code::host_claude_json_path() {
            let target = format!(
                "{}/.claude.json",
                cella_env::claude_code::container_home(remote_user),
            );
            create_opts.mounts.push(MountConfig {
                mount_type: "bind".to_string(),
                source: host_path.to_string_lossy().to_string(),
                target,
                consistency: None,
            });
        }
        if let Some(host_path) = cella_env::claude_code::host_claude_dir() {
            let target = cella_env::claude_code::claude_dir_for_user(remote_user);
            create_opts.mounts.push(MountConfig {
                mount_type: "bind".to_string(),
                source: host_path.to_string_lossy().to_string(),
                target: target.clone(),
                consistency: None,
            });

            // Hidden mount for host plugins (backward sync access)
            if let Some(host_plugins) = cella_env::claude_code::host_plugins_dir() {
                create_opts.mounts.push(MountConfig {
                    mount_type: "bind".to_string(),
                    source: host_plugins.to_string_lossy().to_string(),
                    target: "/tmp/.cella/host-plugins".to_string(),
                    consistency: None,
                });
                // tmpfs shadows the parent bind mount's plugins/ subdirectory
                create_opts.mounts.push(MountConfig {
                    mount_type: "tmpfs".to_string(),
                    source: String::new(),
                    target: format!("{target}/plugins"),
                    consistency: None,
                });
            }
        }
    }

    // Codex: ~/.codex
    if settings.tools.codex.forward_config
        && let Some(host_path) = cella_env::codex::host_codex_dir()
    {
        let target = cella_env::codex::container_codex_dir(remote_user);
        create_opts.mounts.push(MountConfig {
            mount_type: "bind".to_string(),
            source: host_path.to_string_lossy().to_string(),
            target,
            consistency: None,
        });
    }

    // Gemini: ~/.gemini
    if settings.tools.gemini.forward_config
        && let Some(host_path) = cella_env::gemini::host_gemini_dir()
    {
        let target = cella_env::gemini::container_gemini_dir(remote_user);
        create_opts.mounts.push(MountConfig {
            mount_type: "bind".to_string(),
            source: host_path.to_string_lossy().to_string(),
            target,
            consistency: None,
        });
    }

    // Nvim: ~/.config/nvim
    if settings.tools.nvim.forward_config
        && let Some(host_path) =
            cella_env::nvim::host_nvim_config_dir(settings.tools.nvim.config_path.as_deref())
    {
        let target = cella_env::nvim::container_nvim_config_dir(remote_user);
        create_opts.mounts.push(MountConfig {
            mount_type: "bind".to_string(),
            source: host_path.to_string_lossy().to_string(),
            target,
            consistency: None,
        });
    }

    // Tmux: ~/.tmux.conf (file) and/or ~/.config/tmux/ (directory)
    if settings.tools.tmux.forward_config {
        if let Some(host_path) =
            cella_env::tmux::host_tmux_conf(settings.tools.tmux.config_path.as_deref())
        {
            let target = cella_env::tmux::container_tmux_conf(remote_user);
            create_opts.mounts.push(MountConfig {
                mount_type: "bind".to_string(),
                source: host_path.to_string_lossy().to_string(),
                target,
                consistency: None,
            });
        }
        if let Some(host_path) =
            cella_env::tmux::host_tmux_config_dir(settings.tools.tmux.config_path.as_deref())
        {
            let target = cella_env::tmux::container_tmux_config_dir(remote_user);
            create_opts.mounts.push(MountConfig {
                mount_type: "bind".to_string(),
                source: host_path.to_string_lossy().to_string(),
                target,
                consistency: None,
            });
        }
    }
}

// ── Orchestration ────────────────────────────────────────────────────────────

/// Forward config and install AI coding tools (Claude Code, Codex, Gemini).
///
/// Claude Code (curl-based) runs in parallel with npm-based tools (Codex, Gemini).
/// Codex and Gemini run sequentially to avoid npm global lock contention.
pub async fn install_tools(
    client: &DockerClient,
    container_id: &str,
    remote_user: &str,
    settings: &cella_config::Settings,
    probed_env: Option<&ProbedEnv>,
    progress: &ProgressSender,
) {
    // Sequential prerequisite: ensure Node.js/npm once for all npm tools
    let needs_npm = settings.tools.codex.enabled || settings.tools.gemini.enabled;
    let node_available = if needs_npm {
        ensure_node_available(client, container_id, probed_env).await
    } else {
        false
    };

    let any_tool = settings.tools.claude_code.enabled
        || settings.tools.codex.enabled
        || settings.tools.gemini.enabled;

    if !any_tool {
        return;
    }

    // Grouped phase: parallel Claude Code (curl) || npm tools (Codex -> Gemini)
    let phase = progress.phase("Installing tools...");

    let claude_branch = async {
        if settings.tools.claude_code.enabled {
            let step = phase.step("Claude Code");
            install_claude_code(
                client,
                container_id,
                remote_user,
                &settings.tools.claude_code,
                probed_env,
            )
            .await;
            step.finish();
        }
    };

    let npm_branch = async {
        if needs_npm && !node_available {
            warn!("Skipping npm tool installs: Node.js/npm not available");
            return;
        }
        if settings.tools.codex.enabled {
            let step = phase.step("Codex");
            install_codex(
                client,
                container_id,
                remote_user,
                &settings.tools.codex,
                probed_env,
            )
            .await;
            step.finish();
        }
        if settings.tools.gemini.enabled {
            let step = phase.step("Gemini CLI");
            install_gemini(
                client,
                container_id,
                remote_user,
                &settings.tools.gemini,
                probed_env,
            )
            .await;
            step.finish();
        }
    };

    tokio::join!(claude_branch, npm_branch);
    phase.finish();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_exec_env_with_path() {
        let mut env = ProbedEnv::new();
        env.insert("PATH".to_string(), "/usr/bin:/usr/local/bin".to_string());
        let result = tool_exec_env(Some(&env));
        assert!(result.is_some());
        let vec = result.unwrap();
        assert_eq!(vec, vec!["PATH=/usr/bin:/usr/local/bin"]);
    }

    #[test]
    fn tool_exec_env_without_path() {
        let env = ProbedEnv::new();
        let result = tool_exec_env(Some(&env));
        assert!(result.is_none());
    }

    #[test]
    fn tool_exec_env_none() {
        let result = tool_exec_env(None);
        assert!(result.is_none());
    }

    #[test]
    fn tool_shell_cmd_with_probed_path() {
        let mut env = ProbedEnv::new();
        env.insert("PATH".to_string(), "/usr/bin".to_string());
        let cmd = tool_shell_cmd(Some(&env), "echo hello");
        assert_eq!(cmd, vec!["sh", "-c", "echo hello"]);
    }

    #[test]
    fn tool_shell_cmd_without_probed_path() {
        let cmd = tool_shell_cmd(None, "echo hello");
        assert_eq!(cmd, vec!["sh", "-l", "-c", "echo hello"]);
    }

    #[test]
    fn tool_shell_cmd_probed_env_without_path_key() {
        let env = ProbedEnv::new();
        let cmd = tool_shell_cmd(Some(&env), "echo hello");
        assert_eq!(cmd, vec!["sh", "-l", "-c", "echo hello"]);
    }

    #[test]
    fn tool_exec_env_ignores_non_path_keys() {
        let mut env = ProbedEnv::new();
        env.insert("HOME".to_string(), "/home/user".to_string());
        env.insert("SHELL".to_string(), "/bin/bash".to_string());
        let result = tool_exec_env(Some(&env));
        assert!(result.is_none());
    }

    #[test]
    fn tool_exec_env_extracts_only_path() {
        let mut env = ProbedEnv::new();
        env.insert("PATH".to_string(), "/usr/bin".to_string());
        env.insert("HOME".to_string(), "/home/user".to_string());
        let result = tool_exec_env(Some(&env)).unwrap();
        assert_eq!(result.len(), 1);
        assert!(result[0].starts_with("PATH="));
    }

    #[test]
    fn tool_shell_cmd_preserves_complex_inner_command() {
        let complex = "cd /app && npm install && npm run build 2>&1 | tee build.log";
        let mut env = ProbedEnv::new();
        env.insert("PATH".to_string(), "/usr/bin".to_string());
        let cmd = tool_shell_cmd(Some(&env), complex);
        assert_eq!(cmd[2], complex);
    }

    #[test]
    fn tool_shell_cmd_login_shell_for_empty_inner() {
        let cmd = tool_shell_cmd(None, "");
        assert_eq!(cmd, vec!["sh", "-l", "-c", ""]);
    }
}
