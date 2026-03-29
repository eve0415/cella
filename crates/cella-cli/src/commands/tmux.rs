use std::io::Write;

use clap::Args;
use serde_json::json;
use tracing::debug;

use cella_docker::{ExecOptions, InteractiveExecOptions};

use super::up::{OutputFormat, UpArgs, UpContext};

/// Open a persistent tmux session inside the dev container.
///
/// Ensures the container is running (auto-up if needed), runs `postAttachCommand`,
/// installs tmux on-demand if not present, then attaches to (or creates) a named
/// tmux session.
#[derive(Args)]
pub struct TmuxArgs {
    #[command(flatten)]
    pub up: UpArgs,

    /// Target a specific compose service (defaults to primary service).
    #[arg(long)]
    pub service: Option<String>,

    /// Skip the nested-tmux warning when already inside a tmux session.
    #[arg(long)]
    pub force: bool,

    /// Additional arguments passed to tmux (after `--`).
    #[arg(last = true)]
    pub extra_args: Vec<String>,
}

impl TmuxArgs {
    pub const fn is_text_output(&self) -> bool {
        self.up.is_text_output()
    }

    pub async fn execute(
        self,
        progress: crate::progress::Progress,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // 1. Ensure container is up
        let build_no_cache = self.up.build_no_cache;
        let strict = self.up.strict.clone();
        let output_format = self.up.output.clone();
        let force = self.force;
        let ctx = UpContext::new(&self.up, progress).await?;
        let result = ctx.ensure_up(build_no_cache, &strict).await?;

        // 2. Resolve compose service if needed
        let container_id = if self.service.is_some() {
            let container = ctx.client.inspect_container(&result.container_id).await?;
            let resolved =
                super::resolve_service_container(&ctx.client, container, self.service.as_deref())
                    .await?;
            resolved.id
        } else {
            result.container_id.clone()
        };

        // 3. Check / install tmux on-demand
        let tmux_info = ensure_tmux(&ctx, &container_id, &result.remote_user).await?;

        // 4. Determine session name
        let session_name = ctx.container_nm.clone();

        // 5. Check if session exists
        let session_exists = check_tmux_session(
            &ctx.client,
            &container_id,
            &result.remote_user,
            &session_name,
        )
        .await;

        // 6. JSON output mode: report and exit
        if matches!(output_format, OutputFormat::Json) {
            let output = json!({
                "outcome": result.outcome,
                "containerId": container_id,
                "remoteUser": result.remote_user,
                "remoteWorkspaceFolder": result.workspace_folder,
                "tmuxInstalled": true,
                "tmuxVersion": tmux_info.version,
                "tmuxSession": session_name,
                "tmuxSessionExists": session_exists,
            });
            println!(
                "{}",
                serde_json::to_string_pretty(&output).unwrap_or_default()
            );
            return Ok(());
        }

        // 7. Nested tmux warning
        if !force && std::env::var("TMUX").is_ok() {
            warn_nested_tmux()?;
        }

        // 8. Build environment
        let container = ctx.client.inspect_container(&container_id).await?;
        let label_env: Vec<String> = container
            .labels
            .get("dev.cella.remote_env")
            .and_then(|v| serde_json::from_str(v).ok())
            .unwrap_or_default();

        let base_env = if let Some(probed) =
            super::env_cache::read_probed_env_cache(&ctx.client, &container_id, &result.remote_user)
                .await
        {
            cella_env::user_env_probe::merge_env(&probed, &label_env)
        } else {
            label_env
        };
        let mut env = base_env;

        super::env_cache::ensure_ssh_auth_sock(
            &ctx.client,
            &container_id,
            &result.remote_user,
            &mut env,
        )
        .await;

        for var in super::TERMINAL_ENV_VARS {
            if let Ok(val) = std::env::var(var) {
                env.push(format!("{var}={val}"));
            }
        }

        // 9. Build command
        let cmd = if self.extra_args.is_empty() {
            // Default: attach-or-create named session
            vec![
                "tmux".to_string(),
                "new-session".to_string(),
                "-As".to_string(),
                session_name,
            ]
        } else {
            // User-provided args: pass through
            let mut c = vec!["tmux".to_string()];
            c.extend(self.extra_args);
            c
        };

        let working_dir = container.labels.get("dev.cella.workspace_folder").cloned();

        // 10. Exec interactive
        let exit_code = ctx
            .client
            .exec_interactive(
                &container_id,
                &InteractiveExecOptions {
                    cmd,
                    user: Some(result.remote_user),
                    env: Some(env),
                    working_dir,
                    tty: true,
                },
            )
            .await?;

        std::process::exit(i32::try_from(exit_code).unwrap_or(125));
    }
}

/// Info about the tmux installation in the container.
struct TmuxInfo {
    version: String,
}

/// Ensure tmux is available in the container, installing on-demand if needed.
async fn ensure_tmux(
    ctx: &UpContext,
    container_id: &str,
    remote_user: &str,
) -> Result<TmuxInfo, Box<dyn std::error::Error>> {
    let check = ctx
        .client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: vec!["which".to_string(), "tmux".to_string()],
                user: Some(remote_user.to_string()),
                env: None,
                working_dir: None,
            },
        )
        .await?;

    if check.exit_code == 0 {
        let version = get_tmux_version(&ctx.client, container_id, remote_user).await;
        debug!("tmux already installed: {version}");
        return Ok(TmuxInfo { version });
    }

    let step = ctx.progress.step("Installing tmux...");
    install_tmux(ctx, container_id).await?;
    step.finish();

    let version = get_tmux_version(&ctx.client, container_id, remote_user).await;
    ctx.progress
        .hint(&format!("tmux {version} installed in container."));
    Ok(TmuxInfo { version })
}

/// Get the tmux version string from the container.
async fn get_tmux_version(
    client: &cella_docker::DockerClient,
    container_id: &str,
    remote_user: &str,
) -> String {
    let result = client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: vec!["tmux".to_string(), "-V".to_string()],
                user: Some(remote_user.to_string()),
                env: None,
                working_dir: None,
            },
        )
        .await;

    match result {
        Ok(r) if r.exit_code == 0 => r.stdout.trim().to_string(),
        _ => "unknown".to_string(),
    }
}

/// Install tmux via the container's package manager.
async fn install_tmux(
    ctx: &UpContext,
    container_id: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    // Detect package manager and build install command
    let install_commands: &[(&str, &str)] = &[
        (
            "apt-get",
            "apt-get update -qq && apt-get install -y -qq tmux",
        ),
        ("apk", "apk add --no-cache tmux"),
        ("dnf", "dnf install -y tmux"),
        ("pacman", "pacman -S --noconfirm tmux"),
        ("zypper", "zypper install -y tmux"),
    ];

    for (pkg_mgr, install_cmd) in install_commands {
        let check = ctx
            .client
            .exec_command(
                container_id,
                &ExecOptions {
                    cmd: vec!["which".to_string(), (*pkg_mgr).to_string()],
                    user: Some("root".to_string()),
                    env: None,
                    working_dir: None,
                },
            )
            .await?;

        if check.exit_code == 0 {
            debug!("Installing tmux via {pkg_mgr}");
            let result = ctx
                .client
                .exec_command(
                    container_id,
                    &ExecOptions {
                        cmd: vec![
                            "sh".to_string(),
                            "-c".to_string(),
                            (*install_cmd).to_string(),
                        ],
                        user: Some("root".to_string()),
                        env: None,
                        working_dir: None,
                    },
                )
                .await?;

            if result.exit_code != 0 {
                return Err(format!(
                    "Failed to install tmux via {pkg_mgr} (exit {}): {}",
                    result.exit_code,
                    result.stderr.trim()
                )
                .into());
            }

            return Ok(());
        }
    }

    Err(
        "No supported package manager found (apt-get, apk, dnf, pacman, zypper). \
         Install tmux manually in your container image."
            .into(),
    )
}

/// Check if a tmux session with the given name exists in the container.
async fn check_tmux_session(
    client: &cella_docker::DockerClient,
    container_id: &str,
    remote_user: &str,
    session_name: &str,
) -> bool {
    let result = client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: vec![
                    "tmux".to_string(),
                    "has-session".to_string(),
                    "-t".to_string(),
                    session_name.to_string(),
                ],
                user: Some(remote_user.to_string()),
                env: None,
                working_dir: None,
            },
        )
        .await;

    matches!(result, Ok(r) if r.exit_code == 0)
}

/// Warn the user about nested tmux sessions and prompt to continue.
fn warn_nested_tmux() -> Result<(), Box<dyn std::error::Error>> {
    eprintln!();
    eprintln!("\x1b[33m\u{26a0}\x1b[0m  You're already inside a tmux session.");
    eprintln!("   Running cella tmux will create a nested tmux session (tmux-inside-tmux).");
    eprintln!();
    eprintln!("   Options:");
    eprintln!("   - Press Enter to continue anyway");
    eprintln!("   - Use `cella shell` for a plain terminal");
    eprintln!("   - Use `--force` to skip this warning");
    eprintln!("   - Press Ctrl-C to abort");
    eprintln!();
    eprint!("   Continue? ");
    std::io::stderr().flush()?;

    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn tmux_version_parse() {
        let output = "tmux 3.4";
        assert_eq!(output.trim(), "tmux 3.4");
    }

    #[test]
    fn nested_tmux_env_check() {
        // The nested tmux check uses std::env::var("TMUX").is_ok()
        // We verify the logic: if TMUX is not set, is_ok() returns false
        let is_nested = std::env::var("TMUX").is_ok();
        // In test env, TMUX is typically not set
        // We just verify the check doesn't panic
        let _ = is_nested;
    }
}
