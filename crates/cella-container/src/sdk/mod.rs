//! SDK for driving the Apple `container` CLI binary.
//!
//! [`ContainerCli`] wraps the binary and provides typed async methods
//! for each CLI subcommand. All operations shell out to the binary and
//! parse its stdout/stderr.

pub mod run;
pub mod types;

use std::path::{Path, PathBuf};

use cella_backend::BackendError;
use tracing::debug;

use self::run::{run_cli, run_cli_json, run_cli_owned};

/// Handle to a discovered Apple Container CLI binary.
pub struct ContainerCli {
    binary_path: PathBuf,
    version: String,
}

impl ContainerCli {
    /// Create a new handle from a discovered binary path and version string.
    pub const fn new(binary_path: PathBuf, version: String) -> Self {
        Self {
            binary_path,
            version,
        }
    }

    /// Path to the `container` binary.
    pub fn binary_path(&self) -> &Path {
        &self.binary_path
    }

    /// Discovered version string.
    pub fn version(&self) -> &str {
        &self.version
    }

    // -- Container lifecycle operations --

    /// Create a container (without starting it).
    ///
    /// `args` should contain all flags and the image name as the final element.
    /// Returns the container ID (plain text from stdout).
    ///
    /// # Errors
    ///
    /// Returns an error if the CLI exits non-zero or cannot be spawned.
    pub async fn create(&self, args: &[String]) -> Result<String, BackendError> {
        let mut cli_args = vec!["create".to_string()];
        cli_args.extend_from_slice(args);
        let output = run_cli_owned(&self.binary_path, &cli_args).await?;
        if output.exit_code != 0 {
            return Err(BackendError::Runtime(output.stderr.into()));
        }
        Ok(output.stdout.trim().to_string())
    }

    /// Start a stopped container.
    ///
    /// # Errors
    ///
    /// Returns an error if the CLI exits non-zero or cannot be spawned.
    pub async fn start(&self, id: &str) -> Result<(), BackendError> {
        let output = run_cli(&self.binary_path, &["start", id]).await?;
        if output.exit_code != 0 {
            return Err(BackendError::Runtime(output.stderr.into()));
        }
        Ok(())
    }

    /// Stop a running container.
    ///
    /// # Errors
    ///
    /// Returns an error if the CLI exits non-zero or cannot be spawned.
    pub async fn stop(&self, id: &str) -> Result<(), BackendError> {
        let output = run_cli(&self.binary_path, &["stop", id]).await?;
        if output.exit_code != 0 {
            return Err(BackendError::Runtime(output.stderr.into()));
        }
        Ok(())
    }

    /// Remove a container.
    ///
    /// # Errors
    ///
    /// Returns an error if the CLI exits non-zero or cannot be spawned.
    pub async fn rm(&self, id: &str) -> Result<(), BackendError> {
        let output = run_cli(&self.binary_path, &["rm", id]).await?;
        if output.exit_code != 0 {
            return Err(BackendError::Runtime(output.stderr.into()));
        }
        Ok(())
    }

    /// Inspect a container and return its full metadata.
    ///
    /// # Errors
    ///
    /// Returns an error if the container does not exist, the CLI exits
    /// non-zero, or the JSON output cannot be parsed.
    pub async fn inspect(&self, id: &str) -> Result<types::ContainerInspect, BackendError> {
        run_cli_json(&self.binary_path, &["inspect", id, "--format", "json"]).await
    }

    /// List containers, optionally filtering by a label.
    ///
    /// # Errors
    ///
    /// Returns an error if the CLI exits non-zero or JSON parsing fails.
    pub async fn list(
        &self,
        label_filter: Option<&str>,
    ) -> Result<Vec<types::ContainerListEntry>, BackendError> {
        let mut args = vec!["ls", "--format", "json", "--all"];
        if let Some(label) = label_filter {
            args.push("--filter");
            args.push(label);
        }
        run_cli_json(&self.binary_path, &args).await
    }

    /// Fetch the last `tail` lines of container logs.
    ///
    /// # Errors
    ///
    /// Returns an error if the CLI cannot be spawned.
    pub async fn logs(&self, id: &str, tail: u32) -> Result<String, BackendError> {
        let tail_str = tail.to_string();
        let output = run_cli(&self.binary_path, &["logs", id, "--tail", &tail_str]).await?;
        // Logs may come on stderr for some runtimes; combine both.
        let mut combined = output.stdout;
        if !output.stderr.is_empty() {
            if !combined.is_empty() {
                combined.push('\n');
            }
            combined.push_str(&output.stderr);
        }
        Ok(combined)
    }

    // -- Exec operations --

    /// Execute a command inside a container and capture its output.
    ///
    /// Returns `(exit_code, stdout, stderr)`.
    ///
    /// # Errors
    ///
    /// Returns an error if the CLI cannot be spawned.
    pub async fn exec_capture(
        &self,
        id: &str,
        cmd: &[String],
        user: Option<&str>,
        env: Option<&[String]>,
        workdir: Option<&str>,
    ) -> Result<(i64, String, String), BackendError> {
        let mut args = vec!["exec".to_string()];

        if let Some(u) = user {
            args.push("--user".to_string());
            args.push(u.to_string());
        }
        if let Some(vars) = env {
            for var in vars {
                args.push("-e".to_string());
                args.push(var.clone());
            }
        }
        if let Some(wd) = workdir {
            args.push("-w".to_string());
            args.push(wd.to_string());
        }

        args.push(id.to_string());
        for c in cmd {
            args.push(c.clone());
        }

        let output = run_cli_owned(&self.binary_path, &args).await?;
        let exit_code = i64::from(output.exit_code);
        Ok((exit_code, output.stdout, output.stderr))
    }

    // -- Image operations --

    /// Pull an image from a registry.
    ///
    /// # Errors
    ///
    /// Returns an error if the CLI exits non-zero or cannot be spawned.
    pub async fn pull(&self, image: &str) -> Result<(), BackendError> {
        debug!(image, "pulling image");
        let output = run_cli(&self.binary_path, &["image", "pull", image]).await?;
        if output.exit_code != 0 {
            return Err(BackendError::Runtime(output.stderr.into()));
        }
        Ok(())
    }

    /// Build an image from a Dockerfile.
    ///
    /// Returns the image tag.
    ///
    /// # Errors
    ///
    /// Returns `BackendError::ImageBuildFailed` if the build exits non-zero.
    pub async fn build(
        &self,
        context: &Path,
        dockerfile: &str,
        tag: &str,
        args: &[(String, String)],
    ) -> Result<String, BackendError> {
        let mut cli_args = vec![
            "build".to_string(),
            "-f".to_string(),
            dockerfile.to_string(),
            "-t".to_string(),
            tag.to_string(),
        ];
        for (key, value) in args {
            cli_args.push("--build-arg".to_string());
            cli_args.push(format!("{key}={value}"));
        }
        cli_args.push(context.to_string_lossy().into_owned());

        let output = run_cli_owned(&self.binary_path, &cli_args).await?;
        if output.exit_code != 0 {
            return Err(BackendError::ImageBuildFailed {
                message: output.stderr,
            });
        }
        Ok(tag.to_string())
    }

    /// Check whether an image exists locally.
    ///
    /// # Errors
    ///
    /// Returns an error if the CLI cannot be spawned.
    pub async fn image_exists(&self, image: &str) -> Result<bool, BackendError> {
        let output = run_cli(&self.binary_path, &["image", "inspect", image]).await?;
        Ok(output.exit_code == 0)
    }

    /// Inspect an image and return raw JSON output.
    ///
    /// # Errors
    ///
    /// Returns `BackendError::ImageNotFound` if the image does not exist.
    pub async fn image_inspect(&self, image: &str) -> Result<String, BackendError> {
        let output = run_cli(
            &self.binary_path,
            &["image", "inspect", image, "--format", "json"],
        )
        .await?;
        if output.exit_code != 0 {
            return Err(BackendError::ImageNotFound {
                image: image.to_string(),
            });
        }
        Ok(output.stdout)
    }

    /// Create a named volume.
    ///
    /// # Errors
    ///
    /// Returns an error if the CLI exits non-zero or cannot be spawned.
    pub async fn volume_create(&self, name: &str) -> Result<(), BackendError> {
        let output = run_cli(&self.binary_path, &["volume", "create", name]).await?;
        if output.exit_code != 0 {
            return Err(BackendError::Runtime(output.stderr.into()));
        }
        Ok(())
    }
}
