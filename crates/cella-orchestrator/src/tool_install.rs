//! Tool installation helpers for AI coding tools (Claude Code, Codex, Gemini).
//!
//! These functions install and configure AI coding tools inside dev containers.
//! They were extracted from the CLI `up` command to be reusable by both the CLI
//! and daemon.

use std::collections::HashMap;

use cella_backend::{BackendError, ContainerBackend, ExecOptions, ExecResult, MountSpec};
use tracing::{debug, warn};

use crate::progress::{PhaseChildHandle, ProgressSender};
use crate::shell_detect::detect_shell;

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
pub async fn is_alpine_container(client: &dyn ContainerBackend, container_id: &str) -> bool {
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
    client: &dyn ContainerBackend,
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
    client: &dyn ContainerBackend,
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
pub async fn ensure_alpine_claude_deps(client: &dyn ContainerBackend, container_id: &str) -> bool {
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
///
/// Returns `Some(ExecResult)` when the native installer was invoked (whether
/// or not it succeeded), and `None` when the idempotency guard short-circuited
/// because the requested version is already installed. Backend errors during
/// exec are flattened into a synthetic `ExecResult` with exit code `-1` so
/// callers can treat all failure modes uniformly.
pub async fn install_claude_code(
    client: &dyn ContainerBackend,
    container_id: &str,
    remote_user: &str,
    settings: &cella_config::settings::ClaudeCode,
    probed_env: Option<&ProbedEnv>,
) -> Option<ExecResult> {
    if is_claude_code_installed(
        client,
        container_id,
        remote_user,
        &settings.version,
        probed_env,
    )
    .await
    {
        return None;
    }

    let is_alpine = ensure_alpine_claude_deps(client, container_id).await;
    Some(
        run_claude_install(
            client,
            container_id,
            remote_user,
            &settings.version,
            is_alpine,
            probed_env,
        )
        .await,
    )
}

/// Execute the Claude Code install script inside the container.
///
/// Backend errors are converted into a synthetic `ExecResult` with exit code
/// `-1` and the error string placed in `stderr`, so the caller can surface
/// the cause without a separate error path.
pub async fn run_claude_install(
    client: &dyn ContainerBackend,
    container_id: &str,
    remote_user: &str,
    version: &str,
    is_alpine: bool,
    probed_env: Option<&ProbedEnv>,
) -> ExecResult {
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

    client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: vec!["sh".to_string(), "-c".to_string(), install_cmd],
                user: Some(remote_user.to_string()),
                env,
                working_dir: None,
            },
        )
        .await
        .unwrap_or_else(|e| ExecResult {
            exit_code: -1,
            stdout: String::new(),
            stderr: e.to_string(),
        })
}

// ── npm tool helpers ─────────────────────────────────────────────────────────

/// Check if an npm-installed CLI tool is already present at the desired version.
pub async fn is_npm_tool_installed(
    client: &dyn ContainerBackend,
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
/// Returns `BackendError` if the exec command fails to run.
pub async fn npm_install_global(
    client: &dyn ContainerBackend,
    container_id: &str,
    remote_user: &str,
    package: &str,
    version: &str,
    probed_env: Option<&ProbedEnv>,
) -> Result<ExecResult, BackendError> {
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

// ── Codex ────────────────────────────────────────────────────────────────────

/// Ensure bubblewrap is available in the container for Codex sandbox support.
///
/// Checks if `bwrap` is already on PATH. If not, installs the `bubblewrap`
/// package via the system package manager (apt-get or apk). Runs as root.
/// Returns `true` if bwrap is available after the check.
pub async fn ensure_codex_sandbox_deps(client: &dyn ContainerBackend, container_id: &str) -> bool {
    let bwrap_check = client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: vec![
                    "sh".to_string(),
                    "-c".to_string(),
                    "command -v bwrap".to_string(),
                ],
                user: Some("root".to_string()),
                env: None,
                working_dir: None,
            },
        )
        .await;

    if bwrap_check.is_ok_and(|r| r.exit_code == 0) {
        debug!("bubblewrap already installed");
        return true;
    }

    debug!("bubblewrap not found, installing...");
    let install_cmd = if is_alpine_container(client, container_id).await {
        "apk add --no-cache bubblewrap"
    } else {
        "apt-get update -qq && apt-get install -y -qq bubblewrap"
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
            debug!("bubblewrap installed successfully");
            true
        }
        Ok(r) => {
            warn!(
                "bubblewrap installation failed (exit {}): {}",
                r.exit_code,
                r.stderr.trim()
            );
            false
        }
        Err(e) => {
            warn!("bubblewrap installation failed: {e}");
            false
        }
    }
}

/// Install `OpenAI` Codex CLI inside the container via npm.
///
/// Ensures bubblewrap is available for sandbox support, then checks if
/// Codex is already installed before running `npm install -g @openai/codex`.
/// Caller must ensure Node.js/npm are available before calling this.
///
/// Returns `Some(ExecResult)` when npm was invoked (success or non-zero),
/// and `None` when Codex is already present at the requested version.
/// Backend errors are flattened into a synthetic `ExecResult` with exit
/// code `-1` so all failure modes share one shape.
pub async fn install_codex(
    client: &dyn ContainerBackend,
    container_id: &str,
    remote_user: &str,
    settings: &cella_config::settings::Codex,
    probed_env: Option<&ProbedEnv>,
) -> Option<ExecResult> {
    ensure_codex_sandbox_deps(client, container_id).await;

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
        return None;
    }

    debug!("Installing Codex ({})...", settings.version);
    Some(
        npm_install_global(
            client,
            container_id,
            remote_user,
            "@openai/codex",
            &settings.version,
            probed_env,
        )
        .await
        .unwrap_or_else(|e| ExecResult {
            exit_code: -1,
            stdout: String::new(),
            stderr: e.to_string(),
        }),
    )
}

// ── Gemini ───────────────────────────────────────────────────────────────────

/// Install Google Gemini CLI inside the container via npm.
///
/// Checks if already installed, then runs `npm install -g @google/gemini-cli`.
/// Caller must ensure Node.js/npm are available before calling this.
///
/// Returns `Some(ExecResult)` when npm was invoked (success or non-zero),
/// and `None` when Gemini CLI is already present at the requested version.
/// Backend errors are flattened into a synthetic `ExecResult` with exit
/// code `-1` so all failure modes share one shape.
pub async fn install_gemini(
    client: &dyn ContainerBackend,
    container_id: &str,
    remote_user: &str,
    settings: &cella_config::settings::Gemini,
    probed_env: Option<&ProbedEnv>,
) -> Option<ExecResult> {
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
        return None;
    }

    debug!("Installing Gemini CLI ({})...", settings.version);
    Some(
        npm_install_global(
            client,
            container_id,
            remote_user,
            "@google/gemini-cli",
            &settings.version,
            probed_env,
        )
        .await
        .unwrap_or_else(|e| ExecResult {
            exit_code: -1,
            stdout: String::new(),
            stderr: e.to_string(),
        }),
    )
}

// ── Claude Code config helpers ───────────────────────────────────────────────

/// Create a symlink from the host's `.claude` path to the container's so that
/// hardcoded paths in plugin manifests (`installed_plugins.json`, `known_marketplaces.json`)
/// resolve transparently.
///
/// Example: host home `/home/node`, container home `/home/vscode`:
///   `/home/node/.claude` -> `/home/vscode/.claude`
pub async fn create_claude_home_symlink(
    client: &dyn ContainerBackend,
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
pub async fn setup_plugin_manifests(
    client: &dyn ContainerBackend,
    container_id: &str,
    remote_user: &str,
) {
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

/// Build bind/tmpfs mount specs for tool config directories (Claude Code, Codex, Gemini, nvim, tmux).
///
/// Returns a [`Vec<MountSpec>`] rather than mutating [`cella_backend::CreateContainerOptions`]
/// so that both the single-container and compose paths can reuse the same decision logic.
pub fn build_tool_config_mount_specs(
    settings: &cella_config::settings::Settings,
    remote_user: &str,
) -> Vec<MountSpec> {
    let mut out = Vec::new();

    // Claude Code: ~/.claude.json (single file) and ~/.claude/ (directory)
    if settings.tools.claude_code.forward_config {
        if let Some(host_path) = cella_env::claude_code::host_claude_json_path() {
            let target = format!(
                "{}/.claude.json",
                cella_env::claude_code::container_home(remote_user),
            );
            out.push(MountSpec::bind(
                host_path.to_string_lossy().to_string(),
                target,
            ));
        }
        if let Some(host_path) = cella_env::claude_code::host_claude_dir() {
            let target = cella_env::claude_code::claude_dir_for_user(remote_user);
            out.push(MountSpec::bind(
                host_path.to_string_lossy().to_string(),
                target.clone(),
            ));

            // Hidden mount for host plugins (backward sync access)
            if let Some(host_plugins) = cella_env::claude_code::host_plugins_dir() {
                out.push(MountSpec::bind(
                    host_plugins.to_string_lossy().to_string(),
                    "/tmp/.cella/host-plugins".to_string(),
                ));
                // tmpfs shadows the parent bind mount's plugins/ subdirectory
                out.push(MountSpec::tmpfs(format!("{target}/plugins")));
            }
        }
    }

    // Codex: ~/.codex
    if settings.tools.codex.forward_config
        && let Some(host_path) = cella_env::codex::host_codex_dir()
    {
        out.push(MountSpec::bind(
            host_path.to_string_lossy().to_string(),
            cella_env::codex::container_codex_dir(remote_user),
        ));
    }

    // Gemini: ~/.gemini
    if settings.tools.gemini.forward_config
        && let Some(host_path) = cella_env::gemini::host_gemini_dir()
    {
        out.push(MountSpec::bind(
            host_path.to_string_lossy().to_string(),
            cella_env::gemini::container_gemini_dir(remote_user),
        ));
    }

    // Nvim: ~/.config/nvim
    if settings.tools.nvim.forward_config
        && let Some(host_path) =
            cella_env::nvim::host_nvim_config_dir(settings.tools.nvim.config_path.as_deref())
    {
        out.push(MountSpec::bind(
            host_path.to_string_lossy().to_string(),
            cella_env::nvim::container_nvim_config_dir(remote_user),
        ));
    }

    // Tmux: ~/.tmux.conf (file) and/or ~/.config/tmux/ (directory)
    if settings.tools.tmux.forward_config {
        if let Some(host_path) =
            cella_env::tmux::host_tmux_conf(settings.tools.tmux.config_path.as_deref())
        {
            out.push(MountSpec::bind(
                host_path.to_string_lossy().to_string(),
                cella_env::tmux::container_tmux_conf(remote_user),
            ));
        }
        if let Some(host_path) =
            cella_env::tmux::host_tmux_config_dir(settings.tools.tmux.config_path.as_deref())
        {
            out.push(MountSpec::bind(
                host_path.to_string_lossy().to_string(),
                cella_env::tmux::container_tmux_config_dir(remote_user),
            ));
        }
    }

    out
}

// ── Verify & symlink ─────────────────────────────────────────────────────────

/// Outcome of checking whether a tool is callable through `cella exec`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyOutcome {
    /// `<shell> -lc "command -v <bin>"` returned exit 0 — the tool is on the
    /// same PATH that `cella exec` uses, so no remediation is needed.
    Reachable,
    /// The `-lc` probe failed but a login+interactive probe (`-lic`) found the
    /// binary at the contained absolute path. Caller may choose to symlink it
    /// into `/usr/local/bin` so that the `-lc` wrap `cella exec` uses can
    /// find it next time.
    InstalledElsewhere(String),
    /// Neither probe located the binary. The installer did not produce a
    /// reachable file.
    NotInstalled,
    /// A backend error prevented verification from running. Treated as a
    /// hard failure because we cannot tell the user whether the install
    /// worked.
    ProbeError(String),
}

/// Check whether `binary` is callable by a login shell, mirroring the exact
/// wrapping `cella exec` uses at `crates/cella-cli/src/commands/exec.rs`.
///
/// Passes `probed_env`'s `PATH` through `tool_exec_env` so the verification
/// decision matches the real env `cella exec` will pass to `docker exec`,
/// not a stricter default PATH.
pub async fn verify_tool_callable(
    client: &dyn ContainerBackend,
    container_id: &str,
    user: &str,
    shell: &str,
    binary: &str,
    probed_env: Option<&ProbedEnv>,
) -> VerifyOutcome {
    let env = tool_exec_env(probed_env);
    let cmd_str = format!("command -v {binary}");

    // 1. Login-shell probe — matches cella exec's `<shell> -lc ...` wrap.
    let lc_result = client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: vec![shell.to_string(), "-lc".to_string(), cmd_str.clone()],
                user: Some(user.to_string()),
                env: env.clone(),
                working_dir: None,
            },
        )
        .await;
    match lc_result {
        Ok(r) if r.exit_code == 0 => return VerifyOutcome::Reachable,
        Ok(_) => {}
        Err(e) => return VerifyOutcome::ProbeError(e.to_string()),
    }

    // 2. Login+interactive fallback — catches installers that only touched
    //    `.bashrc` / `.zshrc`, which `-lc` does not source.
    let lic_result = client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: vec![shell.to_string(), "-lic".to_string(), cmd_str],
                user: Some(user.to_string()),
                env,
                working_dir: None,
            },
        )
        .await;
    match lic_result {
        Ok(r) if r.exit_code == 0 => VerifyOutcome::InstalledElsewhere(r.stdout.trim().to_string()),
        Ok(_) => VerifyOutcome::NotInstalled,
        Err(e) => VerifyOutcome::ProbeError(e.to_string()),
    }
}

/// Symlink `source_path` to `/usr/local/bin/<binary>` as root.
///
/// `/usr/local/bin` is on the default PATH of every shell in every base image,
/// so placing a symlink there guarantees `cella exec <binary>` resolves.
///
/// Refuses to overwrite a regular file at the target (could be user- or
/// image-provided); replaces existing symlinks via `ln -sfn` so the operation
/// is idempotent across repeated `cella up` runs.
///
/// # Errors
///
/// Returns `Err` with a human-readable reason suitable for `step.fail` when:
///   * a non-symlink already exists at `/usr/local/bin/<binary>`;
///   * the backend exec call fails;
///   * the `ln` invocation exits non-zero.
pub async fn symlink_to_usr_local_bin(
    client: &dyn ContainerBackend,
    container_id: &str,
    binary: &str,
    source_path: &str,
) -> Result<(), String> {
    let target = format!("/usr/local/bin/{binary}");

    // Safety: refuse to overwrite a pre-existing regular file. Replacing a
    // symlink we (or a prior cella run) created is fine.
    let check_cmd = format!("if [ -e {target} ] && [ ! -L {target} ]; then echo regular; fi");
    match client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: vec!["sh".to_string(), "-c".to_string(), check_cmd],
                user: Some("root".to_string()),
                env: None,
                working_dir: None,
            },
        )
        .await
    {
        Ok(r) if r.stdout.contains("regular") => {
            return Err(format!("pre-existing {target} is not a symlink"));
        }
        Ok(_) => {}
        Err(e) => return Err(format!("pre-check failed: {e}")),
    }

    let link_cmd = format!("ln -sfn '{}' {target}", source_path.replace('\'', "'\\''"));
    match client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: vec!["sh".to_string(), "-c".to_string(), link_cmd],
                user: Some("root".to_string()),
                env: None,
                working_dir: None,
            },
        )
        .await
    {
        Ok(r) if r.exit_code == 0 => Ok(()),
        Ok(r) => Err(format!("ln exit {}: {}", r.exit_code, r.stderr.trim())),
        Err(e) => Err(format!("exec failed: {e}")),
    }
}

// ── Orchestration ────────────────────────────────────────────────────────────

/// Shared context for per-tool verified install steps. Bundles the
/// container-targeting args so `verified_install_step` stays under the
/// per-function argument limit and so `install_tools` does not repeat itself.
struct InstallCtx<'a> {
    client: &'a dyn ContainerBackend,
    container_id: &'a str,
    remote_user: &'a str,
    shell: &'a str,
    probed_env: Option<&'a ProbedEnv>,
}

/// Finish a phase-child step after verifying the tool is callable via the same
/// login-shell wrap `cella exec` uses. On `InstalledElsewhere` attempts a
/// `/usr/local/bin` symlink and re-verifies. On any terminal failure, folds
/// the installer's exit code and stderr into the `step.fail` message so the
/// user sees why the `✗` appeared.
///
/// If `install_result` indicates the installer itself exited non-zero, the
/// step fails immediately without even asking `verify_tool_callable` — an
/// older copy of the binary may still be on `PATH` from a previous run, but
/// the upgrade the user asked for did not land.
async fn verified_install_step(
    ctx: &InstallCtx<'_>,
    binary: &str,
    install_result: Option<ExecResult>,
    step: PhaseChildHandle,
) {
    // Short-circuit: if the installer ran and reported a non-zero exit, the
    // requested install/upgrade did not take effect. Do not let a stale
    // binary still on PATH render as ✓.
    if matches!(install_result.as_ref(), Some(r) if r.exit_code != 0) {
        step.fail(&render_failure_reason(
            install_result.as_ref(),
            "installer exited non-zero",
        ));
        return;
    }

    let verify = verify_tool_callable(
        ctx.client,
        ctx.container_id,
        ctx.remote_user,
        ctx.shell,
        binary,
        ctx.probed_env,
    )
    .await;

    match verify {
        VerifyOutcome::Reachable => step.finish(),
        VerifyOutcome::InstalledElsewhere(path) => {
            match symlink_to_usr_local_bin(ctx.client, ctx.container_id, binary, &path).await {
                Ok(()) => {
                    let second = verify_tool_callable(
                        ctx.client,
                        ctx.container_id,
                        ctx.remote_user,
                        ctx.shell,
                        binary,
                        ctx.probed_env,
                    )
                    .await;
                    match second {
                        VerifyOutcome::Reachable => step.finish(),
                        other => step.fail(&render_failure_reason(
                            install_result.as_ref(),
                            &format!("symlink created but still not reachable: {other:?}"),
                        )),
                    }
                }
                Err(e) => step.fail(&render_failure_reason(
                    install_result.as_ref(),
                    &format!("symlink failed: {e}"),
                )),
            }
        }
        VerifyOutcome::NotInstalled => step.fail(&render_failure_reason(
            install_result.as_ref(),
            "install did not produce a reachable binary",
        )),
        VerifyOutcome::ProbeError(e) => step.fail(&render_failure_reason(
            install_result.as_ref(),
            &format!("verification failed: {e}"),
        )),
    }
}

/// Compose the `step.fail` reason string. When the installer exited non-zero,
/// prefix with `"installer exit {code}: {stderr_first_line} — "` (stderr line
/// truncated for readability) and also `warn!` the full stderr so it is
/// captured by tracing consumers.
fn render_failure_reason(install_result: Option<&ExecResult>, reason: &str) -> String {
    match install_result {
        Some(r) if r.exit_code != 0 => {
            warn!(
                "Tool install failed (exit {}): {}\nstderr:\n{}",
                r.exit_code,
                reason,
                r.stderr.trim(),
            );
            let head = r.stderr.lines().next().unwrap_or("").trim();
            let head = if head.len() > 200 { &head[..200] } else { head };
            if head.is_empty() {
                format!("installer exit {} — {reason}", r.exit_code)
            } else {
                format!("installer exit {}: {head} — {reason}", r.exit_code)
            }
        }
        _ => reason.to_string(),
    }
}

/// Forward config and install AI coding tools (Claude Code, Codex, Gemini).
///
/// Claude Code (curl-based) runs in parallel with npm-based tools (Codex, Gemini).
/// Codex and Gemini run sequentially to avoid npm global lock contention.
///
/// After each install attempt, `verified_install_step` confirms the binary is
/// callable via the same login-shell wrap `cella exec` uses. When the tool is
/// installed elsewhere (e.g. `~/.local/bin`, an nvm-managed npm global), a
/// `/usr/local/bin/<tool>` symlink is created so repeated `cella up` runs
/// self-heal. A `✗` is rendered with the installer's exit code + stderr when
/// verification still fails.
pub async fn install_tools(
    client: &dyn ContainerBackend,
    container_id: &str,
    remote_user: &str,
    settings: &cella_config::settings::Settings,
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

    // Detect shell once — verify_tool_callable and cella exec both use `-lc`
    // wrapping through this shell, so the decision stays consistent.
    let shell = detect_shell(client, container_id, remote_user).await;
    let ctx = InstallCtx {
        client,
        container_id,
        remote_user,
        shell: &shell,
        probed_env,
    };

    // Grouped phase: parallel Claude Code (curl) || npm tools (Codex -> Gemini)
    let phase = progress.phase("Installing tools...");

    let claude_branch = async {
        if settings.tools.claude_code.enabled {
            let step = phase.step("Claude Code");
            let install_result = install_claude_code(
                client,
                container_id,
                remote_user,
                &settings.tools.claude_code,
                probed_env,
            )
            .await;
            verified_install_step(&ctx, "claude", install_result, step).await;
        }
    };

    let npm_branch = async {
        if needs_npm && !node_available {
            warn!("Skipping npm tool installs: Node.js/npm not available");
            return;
        }
        if settings.tools.codex.enabled {
            let step = phase.step("Codex");
            let install_result = install_codex(
                client,
                container_id,
                remote_user,
                &settings.tools.codex,
                probed_env,
            )
            .await;
            verified_install_step(&ctx, "codex", install_result, step).await;
        }
        if settings.tools.gemini.enabled {
            let step = phase.step("Gemini CLI");
            let install_result = install_gemini(
                client,
                container_id,
                remote_user,
                &settings.tools.gemini,
                probed_env,
            )
            .await;
            verified_install_step(&ctx, "gemini", install_result, step).await;
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

    // ── MockBackend for ensure_codex_sandbox_deps tests ─────────────────────

    use std::collections::VecDeque;
    use std::io::Write;
    use std::path::Path;
    use std::sync::Mutex;

    use cella_backend::{
        BackendCapabilities, BackendKind, BoxFuture, BuildOptions, ContainerInfo,
        CreateContainerOptions, FileToUpload, ImageDetails, InteractiveExecOptions, Platform,
    };

    /// Minimal mock that replays pre-configured `exec_command` responses in order.
    struct MockBackend {
        responses: Mutex<VecDeque<Result<ExecResult, BackendError>>>,
    }

    impl MockBackend {
        fn new(responses: Vec<Result<ExecResult, BackendError>>) -> Self {
            Self {
                responses: Mutex::new(VecDeque::from(responses)),
            }
        }
    }

    impl ContainerBackend for MockBackend {
        fn kind(&self) -> BackendKind {
            unimplemented!()
        }

        fn capabilities(&self) -> BackendCapabilities {
            unimplemented!()
        }

        fn find_container<'a>(
            &'a self,
            _: &'a Path,
        ) -> BoxFuture<'a, Result<Option<ContainerInfo>, BackendError>> {
            unimplemented!()
        }

        fn create_container<'a>(
            &'a self,
            _: &'a CreateContainerOptions,
        ) -> BoxFuture<'a, Result<String, BackendError>> {
            unimplemented!()
        }

        fn start_container<'a>(&'a self, _: &'a str) -> BoxFuture<'a, Result<(), BackendError>> {
            unimplemented!()
        }

        fn stop_container<'a>(&'a self, _: &'a str) -> BoxFuture<'a, Result<(), BackendError>> {
            unimplemented!()
        }

        fn remove_container<'a>(
            &'a self,
            _: &'a str,
            _: bool,
        ) -> BoxFuture<'a, Result<(), BackendError>> {
            unimplemented!()
        }

        fn inspect_container<'a>(
            &'a self,
            _: &'a str,
        ) -> BoxFuture<'a, Result<ContainerInfo, BackendError>> {
            unimplemented!()
        }

        fn list_cella_containers(
            &self,
            _: bool,
        ) -> BoxFuture<'_, Result<Vec<ContainerInfo>, BackendError>> {
            unimplemented!()
        }

        fn find_compose_service<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
        ) -> BoxFuture<'a, Result<Option<ContainerInfo>, BackendError>> {
            unimplemented!()
        }

        fn find_container_by_label<'a>(
            &'a self,
            _: &'a str,
        ) -> BoxFuture<'a, Result<Option<ContainerInfo>, BackendError>> {
            unimplemented!()
        }

        fn container_logs<'a>(
            &'a self,
            _: &'a str,
            _: u32,
        ) -> BoxFuture<'a, Result<String, BackendError>> {
            unimplemented!()
        }

        fn exec_command<'a>(
            &'a self,
            _container_id: &'a str,
            _opts: &'a ExecOptions,
        ) -> BoxFuture<'a, Result<ExecResult, BackendError>> {
            let response = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("MockBackend: no more responses");
            Box::pin(async move { response })
        }

        fn exec_stream<'a>(
            &'a self,
            _: &'a str,
            _: &'a ExecOptions,
            _: Box<dyn Write + Send + 'a>,
            _: Box<dyn Write + Send + 'a>,
        ) -> BoxFuture<'a, Result<ExecResult, BackendError>> {
            unimplemented!()
        }

        fn exec_interactive<'a>(
            &'a self,
            _: &'a str,
            _: &'a InteractiveExecOptions,
        ) -> BoxFuture<'a, Result<i64, BackendError>> {
            unimplemented!()
        }

        fn exec_detached<'a>(
            &'a self,
            _: &'a str,
            _: &'a ExecOptions,
        ) -> BoxFuture<'a, Result<String, BackendError>> {
            unimplemented!()
        }

        fn pull_image<'a>(&'a self, _: &'a str) -> BoxFuture<'a, Result<(), BackendError>> {
            unimplemented!()
        }

        fn build_image<'a>(
            &'a self,
            _: &'a BuildOptions,
        ) -> BoxFuture<'a, Result<String, BackendError>> {
            unimplemented!()
        }

        fn image_exists<'a>(&'a self, _: &'a str) -> BoxFuture<'a, Result<bool, BackendError>> {
            unimplemented!()
        }

        fn inspect_image_details<'a>(
            &'a self,
            _: &'a str,
        ) -> BoxFuture<'a, Result<ImageDetails, BackendError>> {
            unimplemented!()
        }

        fn upload_files<'a>(
            &'a self,
            _: &'a str,
            _: &'a [FileToUpload],
        ) -> BoxFuture<'a, Result<(), BackendError>> {
            unimplemented!()
        }

        fn ping(&self) -> BoxFuture<'_, Result<(), BackendError>> {
            unimplemented!()
        }

        fn host_gateway(&self) -> &'static str {
            unimplemented!()
        }

        fn detect_platform(&self) -> BoxFuture<'_, Result<Platform, BackendError>> {
            unimplemented!()
        }

        fn detect_container_arch(&self) -> BoxFuture<'_, Result<String, BackendError>> {
            unimplemented!()
        }

        fn inspect_image_env<'a>(
            &'a self,
            _: &'a str,
        ) -> BoxFuture<'a, Result<Vec<String>, BackendError>> {
            unimplemented!()
        }

        fn inspect_image_user<'a>(
            &'a self,
            _: &'a str,
        ) -> BoxFuture<'a, Result<String, BackendError>> {
            unimplemented!()
        }

        fn ensure_network(&self) -> BoxFuture<'_, Result<(), BackendError>> {
            unimplemented!()
        }

        fn ensure_container_network<'a>(
            &'a self,
            _: &'a str,
            _: &'a Path,
        ) -> BoxFuture<'a, Result<(), BackendError>> {
            unimplemented!()
        }

        fn get_container_ip<'a>(
            &'a self,
            _: &'a str,
        ) -> BoxFuture<'a, Result<Option<String>, BackendError>> {
            unimplemented!()
        }

        fn ensure_agent_provisioned<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
            _: bool,
        ) -> BoxFuture<'a, Result<(), BackendError>> {
            unimplemented!()
        }

        fn write_agent_addr<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
            _: &'a str,
        ) -> BoxFuture<'a, Result<(), BackendError>> {
            unimplemented!()
        }

        fn agent_volume_mount(&self) -> (String, String, bool) {
            unimplemented!()
        }

        fn prune_old_agent_versions<'a>(
            &'a self,
            _: &'a str,
        ) -> BoxFuture<'a, Result<(), BackendError>> {
            unimplemented!()
        }
    }

    fn ok_exit(code: i64) -> ExecResult {
        ExecResult {
            exit_code: code,
            stdout: String::new(),
            stderr: String::new(),
        }
    }

    fn fail_exit(code: i64, stderr: &str) -> ExecResult {
        ExecResult {
            exit_code: code,
            stdout: String::new(),
            stderr: stderr.to_string(),
        }
    }

    // Call sequence for ensure_codex_sandbox_deps:
    // 1. exec: "command -v bwrap"          (bwrap check)
    // 2. exec: "test -f /etc/alpine-release" (alpine check, only if bwrap missing)
    // 3. exec: install command               (only if bwrap missing)

    #[tokio::test]
    async fn ensure_codex_sandbox_deps_bwrap_already_installed() {
        // bwrap found on PATH -> return true, no further calls
        let backend = MockBackend::new(vec![Ok(ok_exit(0))]);
        let result = ensure_codex_sandbox_deps(&backend, "test-container").await;
        assert!(result);
    }

    #[tokio::test]
    async fn ensure_codex_sandbox_deps_debian_install_success() {
        let backend = MockBackend::new(vec![
            Ok(ok_exit(1)), // bwrap not found
            Ok(ok_exit(1)), // not alpine (test -f /etc/alpine-release fails)
            Ok(ok_exit(0)), // apt-get install succeeds
        ]);
        let result = ensure_codex_sandbox_deps(&backend, "test-container").await;
        assert!(result);
    }

    #[tokio::test]
    async fn ensure_codex_sandbox_deps_alpine_install_success() {
        let backend = MockBackend::new(vec![
            Ok(ok_exit(1)), // bwrap not found
            Ok(ok_exit(0)), // is alpine (test -f /etc/alpine-release succeeds)
            Ok(ok_exit(0)), // apk add succeeds
        ]);
        let result = ensure_codex_sandbox_deps(&backend, "test-container").await;
        assert!(result);
    }

    #[tokio::test]
    async fn ensure_codex_sandbox_deps_debian_install_failure() {
        let backend = MockBackend::new(vec![
            Ok(ok_exit(1)),                                               // bwrap not found
            Ok(ok_exit(1)),                                               // not alpine
            Ok(fail_exit(100, "E: Unable to locate package bubblewrap")), // apt-get fails
        ]);
        let result = ensure_codex_sandbox_deps(&backend, "test-container").await;
        assert!(!result);
    }

    #[tokio::test]
    async fn ensure_codex_sandbox_deps_alpine_install_failure() {
        let backend = MockBackend::new(vec![
            Ok(ok_exit(1)),                                       // bwrap not found
            Ok(ok_exit(0)),                                       // is alpine
            Ok(fail_exit(1, "ERROR: unable to select packages")), // apk add fails
        ]);
        let result = ensure_codex_sandbox_deps(&backend, "test-container").await;
        assert!(!result);
    }

    #[test]
    fn build_tool_config_mount_specs_returns_empty_when_disabled() {
        let mut settings = cella_config::settings::Settings::default();
        settings.tools.claude_code.forward_config = false;
        settings.tools.codex.forward_config = false;
        settings.tools.gemini.forward_config = false;
        settings.tools.nvim.forward_config = false;
        settings.tools.tmux.forward_config = false;
        let specs = build_tool_config_mount_specs(&settings, "root");
        assert!(
            specs.is_empty(),
            "no mounts when all forward_config=false; got {specs:?}"
        );
    }

    // ── verify_tool_callable ────────────────────────────────────────────────
    //
    // Call sequence:
    //   1. `<shell> -lc "command -v <binary>"` (must match cella exec wrap)
    //   2. `<shell> -lic "command -v <binary>"` (only if 1 exited non-zero)

    fn ok_stdout(code: i64, stdout: &str) -> ExecResult {
        ExecResult {
            exit_code: code,
            stdout: stdout.to_string(),
            stderr: String::new(),
        }
    }

    #[tokio::test]
    async fn verify_tool_callable_reachable_via_lc() {
        let backend = MockBackend::new(vec![Ok(ok_exit(0))]);
        let outcome = verify_tool_callable(
            &backend,
            "test-container",
            "vscode",
            "/bin/bash",
            "claude",
            None,
        )
        .await;
        assert_eq!(outcome, VerifyOutcome::Reachable);
    }

    #[tokio::test]
    async fn verify_tool_callable_installed_elsewhere_via_lic() {
        let backend = MockBackend::new(vec![
            Ok(ok_exit(1)), // -lc: not found
            Ok(ok_stdout(0, "/home/vscode/.local/bin/claude\n")),
        ]);
        let outcome = verify_tool_callable(
            &backend,
            "test-container",
            "vscode",
            "/bin/bash",
            "claude",
            None,
        )
        .await;
        assert_eq!(
            outcome,
            VerifyOutcome::InstalledElsewhere("/home/vscode/.local/bin/claude".to_string()),
        );
    }

    #[tokio::test]
    async fn verify_tool_callable_not_installed() {
        let backend = MockBackend::new(vec![
            Ok(ok_exit(1)), // -lc: not found
            Ok(ok_exit(1)), // -lic: also not found
        ]);
        let outcome = verify_tool_callable(
            &backend,
            "test-container",
            "vscode",
            "/bin/bash",
            "claude",
            None,
        )
        .await;
        assert_eq!(outcome, VerifyOutcome::NotInstalled);
    }

    #[tokio::test]
    async fn verify_tool_callable_probe_error_on_first_call() {
        let backend = MockBackend::new(vec![Err(BackendError::ContainerNotFound {
            identifier: "dead".into(),
        })]);
        let outcome = verify_tool_callable(
            &backend,
            "test-container",
            "vscode",
            "/bin/bash",
            "claude",
            None,
        )
        .await;
        match outcome {
            VerifyOutcome::ProbeError(msg) => assert!(msg.contains("dead")),
            other => panic!("expected ProbeError, got {other:?}"),
        }
    }

    // ── symlink_to_usr_local_bin ────────────────────────────────────────────
    //
    // Call sequence:
    //   1. pre-check: `sh -c "if [ -e /usr/local/bin/X ] && [ ! -L ... ]; then echo regular; fi"`
    //   2. `sh -c "ln -sfn SOURCE /usr/local/bin/X"` (only when pre-check clean)

    #[tokio::test]
    async fn symlink_to_usr_local_bin_success_target_absent() {
        let backend = MockBackend::new(vec![
            Ok(ok_exit(0)), // pre-check: nothing at target
            Ok(ok_exit(0)), // ln -sfn
        ]);
        let result = symlink_to_usr_local_bin(
            &backend,
            "test-container",
            "claude",
            "/home/vscode/.local/bin/claude",
        )
        .await;
        assert!(result.is_ok(), "expected Ok, got {result:?}");
    }

    #[tokio::test]
    async fn symlink_to_usr_local_bin_refuses_regular_file() {
        let backend = MockBackend::new(vec![Ok(ok_stdout(0, "regular\n"))]);
        let err = symlink_to_usr_local_bin(
            &backend,
            "test-container",
            "claude",
            "/home/vscode/.local/bin/claude",
        )
        .await
        .expect_err("should refuse to overwrite regular file");
        assert!(err.contains("not a symlink"), "got: {err}");
    }

    #[tokio::test]
    async fn symlink_to_usr_local_bin_ln_nonzero_exit() {
        let backend = MockBackend::new(vec![
            Ok(ok_exit(0)),                        // pre-check clean
            Ok(fail_exit(1, "Permission denied")), // ln fails
        ]);
        let err = symlink_to_usr_local_bin(
            &backend,
            "test-container",
            "claude",
            "/home/vscode/.local/bin/claude",
        )
        .await
        .expect_err("ln exit 1 should surface");
        assert!(err.contains("Permission denied"), "got: {err}");
    }

    // ── verified_install_step: installer-failed short-circuit ───────────────
    //
    // Regression: a non-zero installer exit should render ✗ even when an
    // older copy of the same binary is still on PATH from a previous run.

    #[tokio::test]
    async fn verified_install_step_installer_nonzero_exits_short_circuits_to_fail() {
        use crate::progress::{ProgressEvent, ProgressSender};

        // Empty MockBackend: short-circuit must not issue any exec calls.
        let backend = MockBackend::new(vec![]);
        let ctx = InstallCtx {
            client: &backend,
            container_id: "test-container",
            remote_user: "vscode",
            shell: "/bin/bash",
            probed_env: None,
        };

        let (tx, mut rx) = tokio::sync::mpsc::channel::<ProgressEvent>(32);
        let sender = ProgressSender::new(tx, false);
        let phase = sender.phase("Installing tools...");
        let step = phase.step("Claude Code");

        let install_result = Some(ExecResult {
            exit_code: 42,
            stdout: String::new(),
            stderr: "network unreachable".into(),
        });
        verified_install_step(&ctx, "claude", install_result, step).await;
        phase.finish();

        // Drain events and assert the child ended with PhaseChildFailed.
        let mut saw_failed = false;
        let mut saw_completed = false;
        while let Ok(ev) = rx.try_recv() {
            match ev {
                ProgressEvent::PhaseChildFailed { message, .. } => {
                    saw_failed = true;
                    assert!(
                        message.contains("42"),
                        "expected installer exit code in message, got: {message}",
                    );
                    assert!(
                        message.contains("network unreachable"),
                        "expected stderr first line in message, got: {message}",
                    );
                }
                ProgressEvent::PhaseChildCompleted { .. } => saw_completed = true,
                _ => {}
            }
        }
        assert!(saw_failed, "expected PhaseChildFailed");
        assert!(
            !saw_completed,
            "must not render ✓ when installer exited non-zero",
        );
    }
}
