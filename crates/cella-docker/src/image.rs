//! Image pull and Dockerfile build operations.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use bollard::image::{BuildImageOptions, CreateImageOptions};
use futures_util::StreamExt;
use tracing::{debug, info};

use crate::CellaDockerError;
use crate::client::DockerClient;

/// Options for building a Docker image from a Dockerfile.
pub struct BuildOptions {
    pub image_name: String,
    pub context_path: PathBuf,
    pub dockerfile: String,
    pub args: HashMap<String, String>,
    pub target: Option<String>,
    pub cache_from: Vec<String>,
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
            from_image: image,
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

    /// Build an image from a Dockerfile.
    ///
    /// # Errors
    ///
    /// Returns error if tar creation, build, or Docker API fails.
    pub async fn build_image(&self, opts: &BuildOptions) -> Result<String, CellaDockerError> {
        info!("Building image: {}", opts.image_name);

        let tar = create_build_context(&opts.context_path)?;

        let build_opts = BuildImageOptions::<String> {
            dockerfile: opts.dockerfile.clone(),
            t: opts.image_name.clone(),
            rm: true,
            target: opts.target.clone().unwrap_or_default(),
            cachefrom: opts.cache_from.clone(),
            buildargs: opts.args.clone(),
            ..Default::default()
        };

        let mut stream = self.inner().build_image(build_opts, None, Some(tar.into()));
        while let Some(result) = stream.next().await {
            match result {
                Ok(output) => {
                    if let Some(stream_msg) = &output.stream {
                        let msg = stream_msg.trim();
                        if !msg.is_empty() {
                            debug!("{msg}");
                        }
                    }
                    if let Some(error) = &output.error {
                        return Err(CellaDockerError::BuildFailed {
                            message: error.clone(),
                        });
                    }
                }
                Err(e) => return Err(CellaDockerError::DockerApi(e)),
            }
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
}

/// Create a tar archive of the build context directory using the system `tar` command.
fn create_build_context(context_path: &Path) -> Result<Vec<u8>, CellaDockerError> {
    let output = std::process::Command::new("tar")
        .args(["cf", "-", "-C"])
        .arg(context_path)
        .arg(".")
        .output()
        .map_err(|source| CellaDockerError::HostCommandFailed {
            command: "tar".to_string(),
            source,
        })?;

    if !output.status.success() {
        return Err(CellaDockerError::BuildFailed {
            message: format!(
                "failed to create build context tar: {}",
                String::from_utf8_lossy(&output.stderr)
            ),
        });
    }

    Ok(output.stdout)
}
