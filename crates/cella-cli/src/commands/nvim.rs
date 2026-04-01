use clap::Args;
use serde_json::json;
use tracing::debug;

use cella_backend::{ExecOptions, InteractiveExecOptions};

use super::up::{OutputFormat, UpArgs, UpContext};

use crate::picker;

/// Open neovim inside the dev container.
///
/// Ensures the container is running (auto-up if needed), runs `postAttachCommand`,
/// installs nvim on-demand if not present, then execs into the container.
#[derive(Args)]
pub struct NvimArgs {
    #[command(flatten)]
    pub up: UpArgs,

    /// Target a specific compose service (defaults to primary service).
    #[arg(long)]
    pub service: Option<String>,

    /// Additional arguments passed to nvim (after `--`).
    #[arg(last = true)]
    pub extra_args: Vec<String>,
}

impl NvimArgs {
    pub const fn is_text_output(&self) -> bool {
        self.up.is_text_output()
    }

    pub async fn execute(
        self,
        progress: crate::progress::Progress,
        backend: Option<&crate::backend::BackendChoice>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // 1. Ensure container is up
        let build_no_cache = self.up.build.build_no_cache;
        let strict = self.up.strict.clone();
        let output_format = self.up.output.clone();
        let mut up = self.up;
        picker::resolve_up_workspace(&mut up).await;
        let ctx = UpContext::new(&up, progress, backend).await?;
        let result = ctx.ensure_up(build_no_cache, &strict).await?;

        // 2. Resolve compose service if needed
        let container_id = if self.service.is_some() {
            let container = ctx.client.inspect_container(&result.container_id).await?;
            let resolved = super::resolve_service_container(
                ctx.client.as_ref(),
                container,
                self.service.as_deref(),
            )
            .await?;
            resolved.id
        } else {
            result.container_id.clone()
        };

        // 3. Check / install nvim on-demand
        let nvim_info = ensure_nvim(&ctx, &container_id, &result.remote_user).await?;

        // 4. JSON output mode: report and exit
        if matches!(output_format, OutputFormat::Json) {
            let output = json!({
                "outcome": result.outcome,
                "containerId": container_id,
                "remoteUser": result.remote_user,
                "remoteWorkspaceFolder": result.workspace_folder,
                "nvimInstalled": true,
                "nvimVersion": nvim_info.version,
            });
            println!(
                "{}",
                serde_json::to_string_pretty(&output).unwrap_or_default()
            );
            return Ok(());
        }

        // 5. Build environment
        let container = ctx.client.inspect_container(&container_id).await?;
        let label_env: Vec<String> = container
            .labels
            .get("dev.cella.remote_env")
            .and_then(|v| serde_json::from_str(v).ok())
            .unwrap_or_default();

        let base_env = if let Some(probed) = super::env_cache::read_probed_env_cache(
            ctx.client.as_ref(),
            &container_id,
            &result.remote_user,
        )
        .await
        {
            cella_env::user_env_probe::merge_env(&probed, &label_env)
        } else {
            label_env
        };
        let mut env = base_env;

        super::env_cache::ensure_ssh_auth_sock(
            ctx.client.as_ref(),
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

        // 6. Build command
        let mut cmd = vec!["nvim".to_string()];
        cmd.extend(self.extra_args);

        let working_dir = container.labels.get("dev.cella.workspace_folder").cloned();

        // 7. Exec interactive
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

/// Info about the nvim installation in the container.
struct NvimInfo {
    version: String,
}

/// Ensure nvim is available in the container, installing on-demand if needed.
async fn ensure_nvim(
    ctx: &UpContext,
    container_id: &str,
    remote_user: &str,
) -> Result<NvimInfo, Box<dyn std::error::Error>> {
    // Check if nvim is already installed
    let check = ctx
        .client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: vec!["which".to_string(), "nvim".to_string()],
                user: Some(remote_user.to_string()),
                env: None,
                working_dir: None,
            },
        )
        .await?;

    if check.exit_code == 0 {
        let version = get_nvim_version(ctx.client.as_ref(), container_id, remote_user).await;
        debug!("nvim already installed: {version}");
        return Ok(NvimInfo { version });
    }

    // Install nvim on-demand
    let step = ctx.progress.step("Installing nvim...");
    install_nvim(ctx, container_id, remote_user).await?;
    step.finish();

    let version = get_nvim_version(ctx.client.as_ref(), container_id, remote_user).await;
    ctx.progress
        .hint(&format!("nvim {version} installed in container."));
    Ok(NvimInfo { version })
}

/// Get the nvim version string from the container.
async fn get_nvim_version(
    client: &dyn cella_backend::ContainerBackend,
    container_id: &str,
    remote_user: &str,
) -> String {
    let result = client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: vec!["nvim".to_string(), "--version".to_string()],
                user: Some(remote_user.to_string()),
                env: None,
                working_dir: None,
            },
        )
        .await;

    match result {
        Ok(r) if r.exit_code == 0 => r
            .stdout
            .lines()
            .next()
            .unwrap_or("unknown")
            .strip_prefix("NVIM ")
            .unwrap_or("unknown")
            .trim()
            .to_string(),
        _ => "unknown".to_string(),
    }
}

/// Install nvim from GitHub releases into the container.
async fn install_nvim(
    ctx: &UpContext,
    container_id: &str,
    remote_user: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    // Detect architecture
    let arch_result = ctx
        .client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: vec!["uname".to_string(), "-m".to_string()],
                user: Some(remote_user.to_string()),
                env: None,
                working_dir: None,
            },
        )
        .await?;

    let arch = arch_result.stdout.trim().to_string();
    debug!("Container architecture: {arch}");

    // Resolve version from settings
    let settings = cella_config::Settings::load(&std::env::current_dir().unwrap_or_default());
    let version = &settings.tools.nvim.version;

    let version_tag = version.as_str();

    // Build download URL based on arch
    let (url, extract_cmd) = match arch.as_str() {
        "x86_64" | "amd64" => {
            let url = format!(
                "https://github.com/neovim/neovim/releases/download/{version_tag}/nvim-linux-x86_64.tar.gz"
            );
            let extract = "tar xzf /tmp/nvim.tar.gz -C /usr/local --strip-components=1";
            (url, extract)
        }
        "aarch64" | "arm64" => {
            let url = format!(
                "https://github.com/neovim/neovim/releases/download/{version_tag}/nvim-linux-aarch64.tar.gz"
            );
            let extract = "tar xzf /tmp/nvim.tar.gz -C /usr/local --strip-components=1";
            (url, extract)
        }
        other => {
            return Err(format!(
                "Unsupported architecture for nvim installation: {other}. \
                 Install nvim manually in your container image."
            )
            .into());
        }
    };

    debug!("Downloading nvim from: {url}");

    // Download and install
    let install_script = format!(
        "curl -fsSL -o /tmp/nvim.tar.gz '{url}' && {extract_cmd} && rm -f /tmp/nvim.tar.gz"
    );

    let install_result = ctx
        .client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: vec!["sh".to_string(), "-c".to_string(), install_script],
                user: Some("root".to_string()),
                env: None,
                working_dir: None,
            },
        )
        .await?;

    if install_result.exit_code != 0 {
        return Err(format!(
            "Failed to install nvim (exit {}): {}",
            install_result.exit_code,
            install_result.stderr.trim()
        )
        .into());
    }

    // Verify installation
    let verify = ctx
        .client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: vec!["nvim".to_string(), "--version".to_string()],
                user: Some(remote_user.to_string()),
                env: None,
                working_dir: None,
            },
        )
        .await?;

    if verify.exit_code != 0 {
        return Err("nvim installed but verification failed. Check container logs.".into());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn nvim_version_parse() {
        let output = "NVIM v0.10.3\nBuild type: Release\nLuaJIT 2.1";
        let version = output
            .lines()
            .next()
            .unwrap_or("unknown")
            .strip_prefix("NVIM ")
            .unwrap_or("unknown")
            .trim();
        assert_eq!(version, "v0.10.3");
    }

    #[test]
    fn nvim_version_parse_empty() {
        let output = "";
        let version = output
            .lines()
            .next()
            .unwrap_or("unknown")
            .strip_prefix("NVIM ")
            .unwrap_or("unknown")
            .trim();
        assert_eq!(version, "unknown");
    }

    #[test]
    fn nvim_version_parse_no_prefix() {
        let output = "v0.9.5";
        let version = output
            .lines()
            .next()
            .unwrap_or("unknown")
            .strip_prefix("NVIM ")
            .unwrap_or("unknown")
            .trim();
        assert_eq!(version, "unknown");
    }

    #[test]
    fn nvim_version_parse_with_whitespace() {
        let output = "NVIM v0.10.0  \n";
        let version = output
            .lines()
            .next()
            .unwrap_or("unknown")
            .strip_prefix("NVIM ")
            .unwrap_or("unknown")
            .trim();
        assert_eq!(version, "v0.10.0");
    }

    #[test]
    fn nvim_version_multiline_first_line() {
        let output = "NVIM v0.11.0\nBuild type: Release\nLuaJIT 2.1.1713484068";
        let version = output
            .lines()
            .next()
            .unwrap_or("unknown")
            .strip_prefix("NVIM ")
            .unwrap_or("unknown")
            .trim();
        assert_eq!(version, "v0.11.0");
    }
}
