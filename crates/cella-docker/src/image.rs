//! Image pull and Dockerfile build operations.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::OnceLock;

use bollard::query_parameters::CreateImageOptions;
use futures_util::StreamExt;
use tracing::{debug, info};

use crate::CellaDockerError;
use crate::client::DockerClient;

/// Extract the user/uid portion from a Docker USER value.
///
/// Docker USER can be `"user"`, `"user:group"`, `"uid"`, or `"uid:gid"`.
/// Returns just the user/uid part, or `"root"` if empty.
pub(crate) fn normalize_user(raw: &str) -> String {
    let user = raw.split(':').next().unwrap_or("");
    if user.is_empty() {
        "root".to_string()
    } else {
        user.to_string()
    }
}

/// Options for building a Docker image from a Dockerfile.
pub struct BuildOptions {
    pub image_name: String,
    pub context_path: PathBuf,
    pub dockerfile: String,
    pub args: HashMap<String, String>,
    pub target: Option<String>,
    pub cache_from: Vec<String>,
    pub options: Vec<String>,
}

/// Locate the docker binary, caching the result.
fn docker_binary() -> Result<&'static str, CellaDockerError> {
    static BINARY: OnceLock<Result<&'static str, String>> = OnceLock::new();
    BINARY
        .get_or_init(|| {
            let output = std::process::Command::new("docker")
                .arg("--version")
                .output();
            match output {
                Ok(o) if o.status.success() => Ok("docker"),
                Ok(o) => Err(format!(
                    "docker --version failed: {}",
                    String::from_utf8_lossy(&o.stderr)
                )),
                Err(e) => Err(format!("docker not found: {e}")),
            }
        })
        .as_ref()
        .copied()
        .map_err(|msg| CellaDockerError::DockerCliNotFound {
            message: msg.clone(),
        })
}

/// Check if `docker buildx` is available, caching the result.
fn has_buildx() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        std::process::Command::new("docker")
            .args(["buildx", "version"])
            .output()
            .is_ok_and(|o| o.status.success())
    })
}

/// Build the argument list for a `docker [buildx] build` invocation.
fn build_command_args(opts: &BuildOptions, use_buildx: bool) -> Vec<String> {
    let mut args = Vec::new();

    if use_buildx {
        args.push("buildx".to_string());
    }
    args.push("build".to_string());

    if use_buildx {
        args.push("--load".to_string());
    }

    args.extend(["-t".to_string(), opts.image_name.clone()]);

    args.extend([
        "-f".to_string(),
        opts.context_path
            .join(&opts.dockerfile)
            .to_string_lossy()
            .to_string(),
    ]);

    for (k, v) in &opts.args {
        args.extend(["--build-arg".to_string(), format!("{k}={v}")]);
    }

    if let Some(target) = &opts.target {
        args.extend(["--target".to_string(), target.clone()]);
    }

    for cf in &opts.cache_from {
        args.extend(["--cache-from".to_string(), cf.clone()]);
    }

    for opt in &opts.options {
        args.push(opt.clone());
    }

    args.push(opts.context_path.to_string_lossy().to_string());
    args
}

impl DockerClient {
    /// Pull an image by reference (e.g., `mcr.microsoft.com/devcontainers/rust:1`).
    ///
    /// # Errors
    ///
    /// Returns `CellaDockerError::DockerApi` if the pull fails.
    pub async fn pull_image(&self, image: &str) -> Result<(), CellaDockerError> {
        info!("Pulling image: {image}");

        let options = CreateImageOptions {
            from_image: Some(image.to_string()),
            ..Default::default()
        };

        let mut stream = self.inner().create_image(Some(options), None, None);
        while let Some(result) = stream.next().await {
            match result {
                Ok(info) => {
                    if let Some(status) = &info.status {
                        debug!("{status}");
                    }
                }
                Err(e) => return Err(CellaDockerError::DockerApi(e)),
            }
        }

        info!("Image pulled: {image}");
        Ok(())
    }

    /// Build an image from a Dockerfile using the docker CLI.
    ///
    /// Prefers `docker buildx build` when available. Falls back to
    /// `docker build` with `DOCKER_BUILDKIT=1` otherwise.
    ///
    /// # Errors
    ///
    /// Returns error if the docker CLI is not found or the build fails.
    pub async fn build_image(&self, opts: &BuildOptions) -> Result<String, CellaDockerError> {
        info!("Building image: {}", opts.image_name);

        let bin = docker_binary()?;
        let use_buildx = has_buildx();
        let args = build_command_args(opts, use_buildx);

        debug!("{bin} {}", args.join(" "));

        let mut cmd = tokio::process::Command::new(bin);
        cmd.args(&args)
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());

        if !use_buildx {
            cmd.env("DOCKER_BUILDKIT", "1");
        }

        let status = cmd
            .status()
            .await
            .map_err(|source| CellaDockerError::HostCommandFailed {
                command: format!("{bin} {}", args.join(" ")),
                source,
            })?;

        if !status.success() {
            return Err(CellaDockerError::BuildFailed {
                message: format!(
                    "docker build exited with code {}",
                    status.code().unwrap_or(-1)
                ),
            });
        }

        info!("Image built: {}", opts.image_name);
        Ok(opts.image_name.clone())
    }

    /// Check if an image exists locally.
    ///
    /// # Errors
    ///
    /// Returns `CellaDockerError::DockerApi` on API errors (not for 404).
    pub async fn image_exists(&self, image: &str) -> Result<bool, CellaDockerError> {
        match self.inner().inspect_image(image).await {
            Ok(_) => Ok(true),
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => Ok(false),
            Err(e) => Err(CellaDockerError::DockerApi(e)),
        }
    }

    /// Inspect an image and return its configured environment variables.
    ///
    /// Returns `Vec<String>` of `KEY=value` entries from the image config.
    ///
    /// # Errors
    ///
    /// Returns `CellaDockerError::DockerApi` on API errors,
    /// `CellaDockerError::ImageNotFound` if the image does not exist.
    pub async fn inspect_image_env(&self, image: &str) -> Result<Vec<String>, CellaDockerError> {
        match self.inner().inspect_image(image).await {
            Ok(details) => Ok(details.config.and_then(|c| c.env).unwrap_or_default()),
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => Err(CellaDockerError::ImageNotFound {
                image: image.to_string(),
            }),
            Err(e) => Err(CellaDockerError::DockerApi(e)),
        }
    }

    /// Inspect an image and return its configured USER (defaulting to `"root"`).
    ///
    /// # Errors
    ///
    /// Returns `CellaDockerError::DockerApi` on API errors,
    /// `CellaDockerError::ImageNotFound` if the image does not exist.
    pub async fn inspect_image_user(&self, image: &str) -> Result<String, CellaDockerError> {
        match self.inner().inspect_image(image).await {
            Ok(details) => {
                let raw = details
                    .config
                    .as_ref()
                    .and_then(|c| c.user.as_deref())
                    .unwrap_or("");
                Ok(normalize_user(raw))
            }
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => Err(CellaDockerError::ImageNotFound {
                image: image.to_string(),
            }),
            Err(e) => Err(CellaDockerError::DockerApi(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn basic_opts() -> BuildOptions {
        BuildOptions {
            image_name: "myimage:latest".to_string(),
            context_path: PathBuf::from("/src/project"),
            dockerfile: "Dockerfile".to_string(),
            args: HashMap::new(),
            target: None,
            cache_from: Vec::new(),
            options: Vec::new(),
        }
    }

    #[test]
    fn build_args_basic() {
        let opts = basic_opts();
        let args = build_command_args(&opts, false);
        assert_eq!(
            args,
            vec![
                "build",
                "-t",
                "myimage:latest",
                "-f",
                "/src/project/Dockerfile",
                "/src/project",
            ]
        );
    }

    #[test]
    fn build_args_with_buildx() {
        let opts = basic_opts();
        let args = build_command_args(&opts, true);
        assert_eq!(args[0], "buildx");
        assert_eq!(args[1], "build");
        assert_eq!(args[2], "--load");
    }

    #[test]
    fn build_args_with_target() {
        let mut opts = basic_opts();
        opts.target = Some("builder".to_string());
        let args = build_command_args(&opts, false);
        assert!(args.contains(&"--target".to_string()));
        assert!(args.contains(&"builder".to_string()));
    }

    #[test]
    fn build_args_with_build_args() {
        let mut opts = basic_opts();
        opts.args
            .insert("NODE_VERSION".to_string(), "20".to_string());
        let args = build_command_args(&opts, false);
        assert!(args.contains(&"--build-arg".to_string()));
        assert!(args.contains(&"NODE_VERSION=20".to_string()));
    }

    #[test]
    fn build_args_with_cache_from() {
        let mut opts = basic_opts();
        opts.cache_from = vec!["myimage:cache".to_string()];
        let args = build_command_args(&opts, false);
        assert!(args.contains(&"--cache-from".to_string()));
        assert!(args.contains(&"myimage:cache".to_string()));
    }

    #[test]
    fn build_args_with_options_passthrough() {
        let mut opts = basic_opts();
        opts.options = vec!["--no-cache".to_string(), "--pull".to_string()];
        let args = build_command_args(&opts, false);
        assert!(args.contains(&"--no-cache".to_string()));
        assert!(args.contains(&"--pull".to_string()));
        // Context path is always last
        assert_eq!(args.last().unwrap(), "/src/project");
    }

    #[test]
    fn normalize_user_plain_username() {
        assert_eq!(normalize_user("vscode"), "vscode");
    }

    #[test]
    fn normalize_user_with_group() {
        assert_eq!(normalize_user("vscode:vscode"), "vscode");
    }

    #[test]
    fn normalize_user_uid_only() {
        assert_eq!(normalize_user("1000"), "1000");
    }

    #[test]
    fn normalize_user_uid_gid() {
        assert_eq!(normalize_user("1000:1000"), "1000");
    }

    #[test]
    fn normalize_user_empty_defaults_to_root() {
        assert_eq!(normalize_user(""), "root");
    }

    #[test]
    fn normalize_user_colon_only_defaults_to_root() {
        assert_eq!(normalize_user(":1000"), "root");
    }
}
