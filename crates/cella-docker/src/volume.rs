//! Docker volume management for the cella-agent binary.
//!
//! The agent binary is stored in a Docker volume (`cella-agent`) and
//! mounted read-only into containers. The volume is versioned by
//! CLI version + architecture.

use bollard::Docker;
use bollard::models::{ContainerCreateBody, VolumeCreateRequest};
use bollard::query_parameters::{
    CreateContainerOptions as BollardCreateOpts, CreateImageOptions, RemoveContainerOptions,
    StartContainerOptions, WaitContainerOptions,
};
use futures_util::StreamExt;
#[cfg(any(not(debug_assertions), test))]
use sha2::{Digest, Sha256};
#[cfg(not(debug_assertions))]
use tracing::warn;
use tracing::{debug, info};

use crate::CellaDockerError;

/// Docker volume name for the agent binary.
pub const AGENT_VOLUME_NAME: &str = "cella-agent";

/// Volume structure path prefix.
const AGENT_PATH_PREFIX: &str = "/cella";

/// Ensure the cella-agent volume exists.
///
/// Creates the volume if it doesn't already exist.
///
/// # Errors
///
/// Returns error if Docker API call fails.
pub async fn ensure_agent_volume(docker: &Docker) -> Result<(), CellaDockerError> {
    // Check if volume already exists
    match docker.inspect_volume(AGENT_VOLUME_NAME).await {
        Ok(_) => {
            debug!("Volume '{AGENT_VOLUME_NAME}' already exists");
            return Ok(());
        }
        Err(bollard::errors::Error::DockerResponseServerError {
            status_code: 404, ..
        }) => {
            // Volume doesn't exist — create it
        }
        Err(e) => return Err(e.into()),
    }

    let config = VolumeCreateRequest {
        name: Some(AGENT_VOLUME_NAME.to_string()),
        labels: Some(
            [
                ("dev.cella.tool".to_string(), "cella".to_string()),
                ("dev.cella.managed".to_string(), "true".to_string()),
            ]
            .into_iter()
            .collect(),
        ),
        ..Default::default()
    };

    docker.create_volume(config).await?;
    info!("Created Docker volume '{AGENT_VOLUME_NAME}'");
    Ok(())
}

/// Get the agent binary path inside the container for a given version and arch.
pub fn agent_binary_path(version: &str, arch: &str) -> String {
    format!("{AGENT_PATH_PREFIX}/v{version}/{arch}/cella-agent")
}

/// Get the browser helper script path inside the container.
pub fn browser_helper_path() -> String {
    format!("{AGENT_PATH_PREFIX}/bin/cella-browser")
}

/// Get the CLI symlink path inside the container.
///
/// This symlink points to the agent binary. When invoked via this path,
/// the agent enters CLI mode for in-container worktree commands.
pub fn cli_symlink_path() -> String {
    format!("{AGENT_PATH_PREFIX}/bin/cella")
}

/// Generate the mount configuration for the agent volume.
///
/// Returns a tuple of (source, target, readonly) for the volume mount.
pub const fn agent_volume_mount() -> (&'static str, &'static str, bool) {
    (AGENT_VOLUME_NAME, AGENT_PATH_PREFIX, true)
}

/// Check if the dev override env var is set.
///
/// When `CELLA_AGENT_PATH` is set, we bind-mount that path
/// instead of using the volume, allowing developers to test
/// local agent builds.
///
/// Only available in debug builds — release builds download
/// the agent from GitHub releases.
#[cfg(debug_assertions)]
pub fn dev_agent_override() -> Option<String> {
    std::env::var("CELLA_AGENT_PATH").ok()
}

/// Detect the target architecture for the agent binary.
///
/// Maps from Rust's `std::env::consts::ARCH` to the volume path convention.
pub fn detect_agent_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        arch => arch, // fallback: use as-is
    }
}

/// Detect the target architecture from the Docker daemon.
///
/// Uses the Docker daemon's reported architecture rather than the host
/// architecture, which correctly handles Docker Desktop on macOS where
/// the daemon runs in a Linux VM matching the default container arch.
///
/// # Errors
///
/// Returns error if the Docker version API call fails.
pub async fn detect_container_arch(docker: &Docker) -> Result<String, CellaDockerError> {
    let version = docker
        .version()
        .await
        .map_err(|e| CellaDockerError::AgentVolume {
            message: format!("failed to detect Docker platform: {e}"),
        })?;
    let arch = version.arch.unwrap_or_default();
    Ok(match arch.as_str() {
        "aarch64" | "arm64" => "aarch64",
        // Default to x86_64 for unknown or x86_64/amd64 architectures
        _ => "x86_64",
    }
    .to_string())
}

/// Generate the cella-browser helper script content.
///
/// This script is placed at `/cella/bin/cella-browser` and set as the
/// `BROWSER` env var. When called, it forwards the URL to the agent
/// which sends it to the host daemon via the control socket.
pub fn browser_helper_script(version: &str, arch: &str) -> Vec<u8> {
    let agent_path = agent_binary_path(version, arch);
    let script = format!(
        r#"#!/bin/sh
# cella browser helper — forwards URLs to host via cella-agent.
exec "{agent_path}" browser-open "$1"
"#
    );
    script.into_bytes()
}

/// Version marker file path inside the volume.
pub const fn version_marker_path() -> &'static str {
    "/cella/.version"
}

/// Generate the version marker content for the agent volume.
pub fn version_marker_content(arch: &str) -> String {
    let version = env!("CARGO_PKG_VERSION");
    format!("{version}/{arch}\n")
}

/// Remove the cella-agent volume.
///
/// # Errors
///
/// Returns error if Docker API call fails.
pub async fn remove_agent_volume(docker: &Docker) -> Result<(), CellaDockerError> {
    match docker
        .remove_volume(
            AGENT_VOLUME_NAME,
            None::<bollard::query_parameters::RemoveVolumeOptions>,
        )
        .await
    {
        Ok(()) => {
            info!("Removed Docker volume '{AGENT_VOLUME_NAME}'");
            Ok(())
        }
        Err(bollard::errors::Error::DockerResponseServerError {
            status_code: 404, ..
        }) => {
            debug!("Volume '{AGENT_VOLUME_NAME}' doesn't exist, nothing to remove");
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

/// Ensure the agent volume is populated with the correct version of the agent binary
/// and browser helper script.
///
/// Steps:
/// 1. Ensure the volume exists
/// 2. Check version marker — if current, return early
/// 3. Get agent binary bytes (debug: local/container build; release: GitHub download)
/// 4. Upload agent binary + browser helper + version marker via temp container
///
/// # Errors
///
/// Returns error if volume creation, agent binary retrieval, or upload fails.
pub async fn ensure_agent_volume_populated(
    docker: &Docker,
    arch: &str,
    skip_checksum: bool,
) -> Result<(), CellaDockerError> {
    ensure_agent_volume(docker).await?;

    let version = env!("CARGO_PKG_VERSION");
    let expected_marker = format!("{version}/{arch}\n");

    if !needs_repopulation(docker, &expected_marker).await? {
        debug!("Agent volume already up-to-date ({version}/{arch})");
        return Ok(());
    }

    info!("Populating agent volume with v{version}/{arch}...");
    populate_volume(docker, version, arch, &expected_marker, skip_checksum).await?;
    info!("Agent volume populated successfully");
    Ok(())
}

/// Check whether repopulation of the agent volume is needed.
///
/// In debug builds, always returns `true` (code changes without version
/// bumps would otherwise leave a stale agent binary in the volume).
/// In release builds, compares the volume version marker against the expected value.
async fn needs_repopulation(
    docker: &Docker,
    expected_marker: &str,
) -> Result<bool, CellaDockerError> {
    if cfg!(debug_assertions) {
        return Ok(true);
    }
    let up_to_date = check_volume_version(docker, expected_marker).await?;
    Ok(!up_to_date)
}

/// Fetch agent bytes, build browser script, and upload everything to the volume.
async fn populate_volume(
    docker: &Docker,
    version: &str,
    arch: &str,
    expected_marker: &str,
    skip_checksum: bool,
) -> Result<(), CellaDockerError> {
    let agent_bytes = get_agent_binary_bytes(docker, arch, skip_checksum).await?;
    let browser_script = browser_helper_script(version, arch);

    upload_to_volume(
        docker,
        version,
        arch,
        &agent_bytes,
        &browser_script,
        expected_marker,
    )
    .await
}

/// Pull an image if it's not already available locally.
async fn ensure_image_pulled(docker: &Docker, image: &str) -> Result<(), CellaDockerError> {
    // Check if image exists locally
    if docker.inspect_image(image).await.is_ok() {
        return Ok(());
    }

    info!("Pulling image {image}...");
    let options = CreateImageOptions {
        from_image: Some(image.to_string()),
        ..Default::default()
    };

    let mut stream = docker.create_image(Some(options), None, None);
    while let Some(result) = stream.next().await {
        match result {
            Ok(info) => {
                if let Some(status) = &info.status {
                    debug!("{status}");
                }
            }
            Err(e) => {
                return Err(CellaDockerError::AgentVolume {
                    message: format!("failed to pull {image}: {e}"),
                });
            }
        }
    }
    Ok(())
}

/// Check if the volume's version marker matches the expected content.
async fn check_volume_version(docker: &Docker, expected: &str) -> Result<bool, CellaDockerError> {
    ensure_image_pulled(docker, "alpine:3").await?;

    let container_name = "cella-volume-check";

    // Remove stale check container if it exists
    let _ = docker
        .remove_container(
            container_name,
            Some(RemoveContainerOptions {
                force: true,
                ..Default::default()
            }),
        )
        .await;

    let config = ContainerCreateBody {
        image: Some("alpine:3".to_string()),
        cmd: Some(vec!["cat".to_string(), "/cella/.version".to_string()]),
        host_config: Some(bollard::models::HostConfig {
            mounts: Some(vec![bollard::models::Mount {
                target: Some("/cella".to_string()),
                source: Some(AGENT_VOLUME_NAME.to_string()),
                typ: Some(bollard::models::MountTypeEnum::VOLUME),
                read_only: Some(true),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    };

    docker
        .create_container(
            Some(BollardCreateOpts {
                name: Some(container_name.to_string()),
                ..Default::default()
            }),
            config,
        )
        .await?;

    docker
        .start_container(container_name, None::<StartContainerOptions>)
        .await?;

    // Wait for container to finish
    let mut wait_stream =
        docker.wait_container(container_name, Some(WaitContainerOptions::default()));
    while let Some(result) = wait_stream.next().await {
        if !result.is_ok_and(|resp| resp.status_code == 0) {
            let _ = docker
                .remove_container(
                    container_name,
                    Some(RemoveContainerOptions {
                        force: true,
                        ..Default::default()
                    }),
                )
                .await;
            return Ok(false);
        }
    }

    // Read logs for the version content
    let log_opts = bollard::query_parameters::LogsOptions {
        stdout: true,
        ..Default::default()
    };
    let mut log_stream = docker.logs(container_name, Some(log_opts));
    let mut output = String::new();
    while let Some(Ok(chunk)) = log_stream.next().await {
        output.push_str(&chunk.to_string());
    }

    let _ = docker
        .remove_container(
            container_name,
            Some(RemoveContainerOptions {
                force: true,
                ..Default::default()
            }),
        )
        .await;

    Ok(output.trim() == expected.trim())
}

/// Get the agent binary bytes from the appropriate source.
///
/// Debug builds:
/// 1. `CELLA_AGENT_PATH` env var — explicit override (escape hatch)
/// 2. On Linux — use local `cella-agent` from `target/debug/`
/// 3. On non-Linux — build in temp Rust container (cross-compile)
///
/// Release builds:
/// 1. Download from GitHub releases
async fn get_agent_binary_bytes(
    docker: &Docker,
    arch: &str,
    skip_checksum: bool,
) -> Result<Vec<u8>, CellaDockerError> {
    #[cfg(debug_assertions)]
    {
        let _ = (arch, skip_checksum); // only used in release builds

        // Check for CELLA_AGENT_PATH override (fastest dev iteration)
        if let Some(path) = dev_agent_override() {
            info!("Using agent binary from CELLA_AGENT_PATH: {path}");
            return std::fs::read(&path).map_err(|e| CellaDockerError::AgentVolume {
                message: format!("failed to read CELLA_AGENT_PATH ({path}): {e}"),
            });
        }

        // On Linux, the local binary is already the right platform
        if std::env::consts::OS == "linux"
            && let Some(path) = detect_sibling_agent_binary()
        {
            info!("Using agent binary from build output: {}", path.display());
            return std::fs::read(&path).map_err(|e| CellaDockerError::AgentVolume {
                message: format!("failed to read agent binary at {}: {e}", path.display()),
            });
        }

        // Non-Linux (macOS) or binary not found: build in temp container
        info!(
            "Building cella-agent in container (host OS: {})...",
            std::env::consts::OS
        );
        return build_agent_in_container(docker).await;
    }

    #[cfg(not(debug_assertions))]
    {
        let _ = docker; // suppress unused warning in release
        download_agent_from_release(arch, skip_checksum).await
    }
}

/// Verify that a byte slice matches an expected SHA-256 hex digest.
#[cfg(any(not(debug_assertions), test))]
fn verify_agent_checksum(bytes: &[u8], expected: &str) -> Result<(), CellaDockerError> {
    let actual = hex::encode(Sha256::digest(bytes));
    if actual != expected {
        return Err(CellaDockerError::AgentChecksumMismatch {
            expected: expected.to_owned(),
            actual,
        });
    }
    Ok(())
}

/// Fetch the `SHA256SUMS` file from the GitHub release and extract the expected
/// hash for the given artifact.
#[cfg(not(debug_assertions))]
async fn fetch_expected_checksum(
    version: &str,
    artifact_name: &str,
) -> Result<String, CellaDockerError> {
    let url = format!("https://github.com/eve0415/cella/releases/download/v{version}/SHA256SUMS");
    debug!("Fetching SHA256SUMS from {url}");

    let response = reqwest::get(&url)
        .await
        .map_err(|e| CellaDockerError::AgentVolume {
            message: format!("failed to fetch SHA256SUMS from {url}: {e}"),
        })?;

    if !response.status().is_success() {
        return Err(CellaDockerError::AgentVolume {
            message: format!(
                "SHA256SUMS download failed: HTTP {} from {url}",
                response.status()
            ),
        });
    }

    let body = response
        .text()
        .await
        .map_err(|e| CellaDockerError::AgentVolume {
            message: format!("failed to read SHA256SUMS response: {e}"),
        })?;

    parse_sha256sums(&body, artifact_name)
}

/// Parse a `SHA256SUMS` file and return the hash for the given artifact name.
#[cfg(any(not(debug_assertions), test))]
fn parse_sha256sums(contents: &str, artifact_name: &str) -> Result<String, CellaDockerError> {
    for line in contents.lines() {
        // Format: "<hex_hash>  <filename>" (two spaces) or "<hex_hash> <filename>"
        let Some((hash, name)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        if name.trim() == artifact_name {
            return Ok(hash.to_owned());
        }
    }
    Err(CellaDockerError::AgentVolume {
        message: format!("artifact '{artifact_name}' not found in SHA256SUMS"),
    })
}

/// Download the agent binary from the matching GitHub release.
#[cfg(not(debug_assertions))]
async fn download_agent_from_release(
    arch: &str,
    skip_checksum: bool,
) -> Result<Vec<u8>, CellaDockerError> {
    let version = env!("CARGO_PKG_VERSION");
    let artifact_name = format!("cella-agent-{arch}");
    let url =
        format!("https://github.com/eve0415/cella/releases/download/v{version}/{artifact_name}");
    info!("Downloading {artifact_name} v{version} from GitHub releases...");

    let response = reqwest::get(&url)
        .await
        .map_err(|e| CellaDockerError::AgentVolume {
            message: format!("failed to download agent binary from {url}: {e}"),
        })?;

    if !response.status().is_success() {
        return Err(CellaDockerError::AgentVolume {
            message: format!(
                "agent binary download failed: HTTP {} from {url}",
                response.status()
            ),
        });
    }

    let bytes = response
        .bytes()
        .await
        .map_err(|e| CellaDockerError::AgentVolume {
            message: format!("failed to read agent binary response: {e}"),
        })?;

    info!("Downloaded {artifact_name} ({} bytes)", bytes.len());

    if skip_checksum {
        warn!("Skipping SHA256 checksum verification for {artifact_name} (--skip-checksum)");
    } else {
        let expected = fetch_expected_checksum(version, &artifact_name).await?;
        verify_agent_checksum(&bytes, &expected)?;
        info!("SHA256 checksum verified for {artifact_name}");
    }

    Ok(bytes.to_vec())
}

/// Try to find the cella-agent binary next to the running cella binary.
///
/// Only valid on Linux where the local build produces a Linux binary.
#[cfg(debug_assertions)]
fn detect_sibling_agent_binary() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let agent_path = dir.join("cella-agent");
    if agent_path.exists() {
        Some(agent_path)
    } else {
        None
    }
}

/// Force-remove a container, ignoring errors.
#[cfg(debug_assertions)]
async fn force_remove(docker: &Docker, container_name: &str) {
    let _ = docker
        .remove_container(
            container_name,
            Some(RemoveContainerOptions {
                force: true,
                ..Default::default()
            }),
        )
        .await;
}

/// Wait for a build container to complete and return an error if it fails.
#[cfg(debug_assertions)]
async fn await_build_container(
    docker: &Docker,
    container_name: &str,
) -> Result<(), CellaDockerError> {
    let mut wait_stream =
        docker.wait_container(container_name, Some(WaitContainerOptions::default()));

    while let Some(result) = wait_stream.next().await {
        match result {
            Ok(resp) => {
                if resp.status_code != 0 {
                    let log_opts = bollard::query_parameters::LogsOptions {
                        stdout: true,
                        stderr: true,
                        tail: "30".to_string(),
                        ..Default::default()
                    };
                    let mut log_stream = docker.logs(container_name, Some(log_opts));
                    let mut output = String::new();
                    while let Some(Ok(chunk)) = log_stream.next().await {
                        output.push_str(&chunk.to_string());
                    }

                    force_remove(docker, container_name).await;

                    return Err(CellaDockerError::AgentVolume {
                        message: format!(
                            "agent build failed (exit code {}):\n{output}",
                            resp.status_code
                        ),
                    });
                }
            }
            Err(e) => {
                force_remove(docker, container_name).await;
                return Err(CellaDockerError::AgentVolume {
                    message: format!("build container wait failed: {e}"),
                });
            }
        }
    }
    Ok(())
}

/// Download a single file from a container, extracting it from the tar stream.
#[cfg(debug_assertions)]
async fn download_binary_from_container(
    docker: &Docker,
    container_name: &str,
    container_path: &str,
    filename: &str,
) -> Result<Vec<u8>, CellaDockerError> {
    let tar_stream = docker.download_from_container(
        container_name,
        Some(bollard::query_parameters::DownloadFromContainerOptions {
            path: container_path.to_string(),
        }),
    );

    let mut buf = Vec::new();
    let mut stream = tar_stream;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| CellaDockerError::AgentVolume {
            message: format!("download from container failed: {e}"),
        })?;
        buf.extend_from_slice(&chunk);
    }

    extract_file_from_tar(&buf, filename)
}

/// Build cella-agent inside a temp Rust container with workspace source bind-mounted.
///
/// Used on non-Linux hosts (macOS) where `cargo build` produces a native binary
/// that won't run inside Linux containers.
#[cfg(debug_assertions)]
async fn build_agent_in_container(docker: &Docker) -> Result<Vec<u8>, CellaDockerError> {
    let container_name = "cella-agent-build";

    // Find workspace root by walking up from current_exe
    let workspace_root = find_workspace_root().ok_or_else(|| CellaDockerError::AgentVolume {
        message: "cannot find workspace root (no Cargo.toml with [workspace])".to_string(),
    })?;

    info!(
        "Building agent from workspace at {}",
        workspace_root.display()
    );

    ensure_image_pulled(docker, "rust:slim").await?;
    force_remove(docker, container_name).await;

    // Create build container with workspace bind-mounted
    let config = ContainerCreateBody {
        image: Some("rust:slim".to_string()),
        cmd: Some(vec![
            "cargo".to_string(),
            "build".to_string(),
            "-p".to_string(),
            "cella-agent".to_string(),
            "--release".to_string(),
        ]),
        working_dir: Some("/src".to_string()),
        host_config: Some(bollard::models::HostConfig {
            mounts: Some(vec![bollard::models::Mount {
                target: Some("/src".to_string()),
                source: Some(workspace_root.to_string_lossy().to_string()),
                typ: Some(bollard::models::MountTypeEnum::BIND),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    };

    docker
        .create_container(
            Some(BollardCreateOpts {
                name: Some(container_name.to_string()),
                ..Default::default()
            }),
            config,
        )
        .await?;

    docker
        .start_container(container_name, None::<StartContainerOptions>)
        .await?;

    await_build_container(docker, container_name).await?;

    let agent_bytes = download_binary_from_container(
        docker,
        container_name,
        "/src/target/release/cella-agent",
        "cella-agent",
    )
    .await?;

    force_remove(docker, container_name).await;

    info!(
        "Agent binary built successfully ({} bytes)",
        agent_bytes.len()
    );
    Ok(agent_bytes)
}

/// Extract a file by name from a tar archive.
#[cfg(debug_assertions)]
fn extract_file_from_tar(tar_bytes: &[u8], filename: &str) -> Result<Vec<u8>, CellaDockerError> {
    let mut archive = tar::Archive::new(tar_bytes);
    for entry in archive
        .entries()
        .map_err(|e| CellaDockerError::AgentVolume {
            message: format!("tar entries: {e}"),
        })?
    {
        let mut entry = entry.map_err(|e| CellaDockerError::AgentVolume {
            message: format!("tar entry: {e}"),
        })?;
        let path = entry.path().map_err(|e| CellaDockerError::AgentVolume {
            message: format!("tar path: {e}"),
        })?;
        if path
            .file_name()
            .is_some_and(|n| n.to_string_lossy() == filename)
        {
            let mut buf = Vec::new();
            std::io::Read::read_to_end(&mut entry, &mut buf).map_err(|e| {
                CellaDockerError::AgentVolume {
                    message: format!("tar read: {e}"),
                }
            })?;
            return Ok(buf);
        }
    }
    Err(CellaDockerError::AgentVolume {
        message: format!("file '{filename}' not found in tar archive"),
    })
}

/// Find the workspace root by walking up from the current exe looking for
/// a `Cargo.toml` that contains `[workspace]`.
#[cfg(debug_assertions)]
fn find_workspace_root() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let mut dir = exe.parent()?;

    // Walk up from the exe dir (typically target/debug/) looking for workspace root
    for _ in 0..10 {
        let cargo_toml = dir.join("Cargo.toml");
        if cargo_toml.exists()
            && let Ok(content) = std::fs::read_to_string(&cargo_toml)
            && content.contains("[workspace]")
        {
            return Some(dir.to_path_buf());
        }
        dir = dir.parent()?;
    }
    None
}

/// Upload agent binary, browser helper, and version marker to the volume.
async fn upload_to_volume(
    docker: &Docker,
    version: &str,
    arch: &str,
    agent_bytes: &[u8],
    browser_script: &[u8],
    version_marker: &str,
) -> Result<(), CellaDockerError> {
    ensure_image_pulled(docker, "alpine:3").await?;

    let container_name = "cella-volume-populate";

    // Remove stale container if it exists
    let _ = docker
        .remove_container(
            container_name,
            Some(RemoveContainerOptions {
                force: true,
                ..Default::default()
            }),
        )
        .await;

    let config = ContainerCreateBody {
        image: Some("alpine:3".to_string()),
        cmd: Some(vec!["sleep".to_string(), "30".to_string()]),
        host_config: Some(bollard::models::HostConfig {
            mounts: Some(vec![bollard::models::Mount {
                target: Some("/cella".to_string()),
                source: Some(AGENT_VOLUME_NAME.to_string()),
                typ: Some(bollard::models::MountTypeEnum::VOLUME),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    };

    docker
        .create_container(
            Some(BollardCreateOpts {
                name: Some(container_name.to_string()),
                ..Default::default()
            }),
            config,
        )
        .await?;

    docker
        .start_container(container_name, None::<StartContainerOptions>)
        .await?;

    // Build tar archive with all files
    let tar_bytes = build_volume_tar(version, arch, agent_bytes, browser_script, version_marker)?;

    // Upload to container
    docker
        .upload_to_container(
            container_name,
            Some(bollard::query_parameters::UploadToContainerOptions {
                path: "/".to_string(),
                ..Default::default()
            }),
            bollard::body_full(tar_bytes.into()),
        )
        .await?;

    // Stop and remove
    let _ = docker
        .stop_container(
            container_name,
            None::<bollard::query_parameters::StopContainerOptions>,
        )
        .await;
    let _ = docker
        .remove_container(
            container_name,
            Some(RemoveContainerOptions {
                force: true,
                ..Default::default()
            }),
        )
        .await;

    Ok(())
}

/// Build a tar archive containing agent binary, browser helper, and version marker.
fn build_volume_tar(
    version: &str,
    arch: &str,
    agent_bytes: &[u8],
    browser_script: &[u8],
    version_marker: &str,
) -> Result<Vec<u8>, CellaDockerError> {
    let mut buf = Vec::new();
    {
        let mut archive = tar::Builder::new(&mut buf);

        // Agent binary: /cella/v{version}/{arch}/cella-agent
        let agent_path = format!("cella/v{version}/{arch}/cella-agent");
        let mut header = tar::Header::new_gnu();
        header.set_size(agent_bytes.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        archive
            .append_data(&mut header, &agent_path, agent_bytes)
            .map_err(|e| CellaDockerError::AgentVolume {
                message: format!("tar append agent: {e}"),
            })?;

        // Browser helper: /cella/bin/cella-browser
        let mut header = tar::Header::new_gnu();
        header.set_size(browser_script.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        archive
            .append_data(&mut header, "cella/bin/cella-browser", browser_script)
            .map_err(|e| CellaDockerError::AgentVolume {
                message: format!("tar append browser: {e}"),
            })?;

        // CLI symlink: /cella/bin/cella -> agent binary
        // When invoked as "cella", the agent enters CLI mode for in-container commands.
        let mut header = tar::Header::new_gnu();
        let link_target = format!("/cella/v{version}/{arch}/cella-agent");
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        header.set_mode(0o755);
        header.set_cksum();
        archive
            .append_link(&mut header, "cella/bin/cella", &link_target)
            .map_err(|e| CellaDockerError::AgentVolume {
                message: format!("tar append cella symlink: {e}"),
            })?;

        // Version marker: /cella/.version
        let marker_bytes = version_marker.as_bytes();
        let mut header = tar::Header::new_gnu();
        header.set_size(marker_bytes.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        archive
            .append_data(&mut header, "cella/.version", marker_bytes)
            .map_err(|e| CellaDockerError::AgentVolume {
                message: format!("tar append version: {e}"),
            })?;

        archive
            .finish()
            .map_err(|e| CellaDockerError::AgentVolume {
                message: format!("tar finish: {e}"),
            })?;
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_binary_path_format() {
        assert_eq!(
            agent_binary_path("0.1.0", "x86_64"),
            "/cella/v0.1.0/x86_64/cella-agent"
        );
    }

    #[test]
    fn browser_helper_path_format() {
        assert_eq!(browser_helper_path(), "/cella/bin/cella-browser");
    }

    #[test]
    fn agent_volume_mount_values() {
        let (source, target, ro) = agent_volume_mount();
        assert_eq!(source, "cella-agent");
        assert_eq!(target, "/cella");
        assert!(ro);
    }

    #[test]
    fn browser_script_contains_agent_path() {
        let script = browser_helper_script("0.1.0", "x86_64");
        let content = String::from_utf8(script).unwrap();
        assert!(content.contains("/cella/v0.1.0/x86_64/cella-agent"));
        assert!(content.contains("browser-open"));
    }

    #[test]
    fn version_marker_content_format() {
        let content = version_marker_content("x86_64");
        assert!(content.contains('/'));
        assert!(content.ends_with('\n'));
        assert!(content.contains("x86_64"));
    }

    #[test]
    fn detect_arch_returns_known() {
        let arch = detect_agent_arch();
        assert!(!arch.is_empty());
    }

    #[test]
    fn build_volume_tar_creates_valid_archive() {
        let agent_bytes = b"#!/bin/sh\necho agent";
        let browser_bytes = b"#!/bin/sh\necho browser";
        let marker = "0.1.0/aarch64\n";

        let tar_bytes =
            build_volume_tar("0.1.0", "aarch64", agent_bytes, browser_bytes, marker).unwrap();

        // Verify tar contains expected entries
        let mut archive = tar::Archive::new(tar_bytes.as_slice());
        let entries: Vec<String> = archive
            .entries()
            .unwrap()
            .filter_map(|e| {
                e.ok()
                    .and_then(|entry| entry.path().ok().map(|p| p.to_string_lossy().to_string()))
            })
            .collect();

        assert!(entries.iter().any(|e| e.contains("cella-agent")));
        assert!(entries.iter().any(|e| e.contains("cella-browser")));
        assert!(entries.iter().any(|e| e.contains(".version")));
        // CLI symlink: cella/bin/cella
        assert!(entries.iter().any(|e| e == "cella/bin/cella"));
    }

    #[test]
    fn cli_symlink_path_format() {
        assert_eq!(cli_symlink_path(), "/cella/bin/cella");
    }

    #[test]
    fn build_volume_tar_cella_symlink_points_to_agent() {
        let agent_bytes = b"fake-binary";
        let browser_bytes = b"#!/bin/sh";
        let marker = "0.1.0/x86_64\n";

        let tar_bytes =
            build_volume_tar("0.1.0", "x86_64", agent_bytes, browser_bytes, marker).unwrap();

        let mut archive = tar::Archive::new(tar_bytes.as_slice());
        for entry in archive.entries().unwrap() {
            let entry = entry.unwrap();
            let path = entry.path().unwrap().to_string_lossy().to_string();
            if path == "cella/bin/cella" {
                assert_eq!(entry.header().entry_type(), tar::EntryType::Symlink);
                let link = entry.link_name().unwrap().unwrap();
                assert_eq!(link.to_string_lossy(), "/cella/v0.1.0/x86_64/cella-agent");
                return;
            }
        }
        panic!("cella/bin/cella symlink not found in tar");
    }

    #[test]
    fn build_volume_tar_agent_is_executable() {
        let agent_bytes = b"fake-binary";
        let browser_bytes = b"#!/bin/sh";
        let marker = "0.1.0/x86_64\n";

        let tar_bytes =
            build_volume_tar("0.1.0", "x86_64", agent_bytes, browser_bytes, marker).unwrap();

        let mut archive = tar::Archive::new(tar_bytes.as_slice());
        for entry in archive.entries().unwrap() {
            let entry = entry.unwrap();
            let path = entry.path().unwrap().to_string_lossy().to_string();
            if path.contains("cella-agent") {
                assert_eq!(entry.header().mode().unwrap(), 0o755);
            }
        }
    }

    #[test]
    fn detect_sibling_agent_binary_returns_path_or_none() {
        // Should not panic regardless of whether the binary exists
        let result = detect_sibling_agent_binary();
        if let Some(path) = &result {
            assert!(path.exists());
            assert!(path.ends_with("cella-agent"));
        }
    }

    #[test]
    fn find_workspace_root_finds_cella() {
        let root = find_workspace_root();
        // Should find the workspace root (this test runs from within the workspace)
        if let Some(path) = &root {
            assert!(path.join("Cargo.toml").exists());
            assert!(path.join("crates").exists());
        }
    }

    #[test]
    fn extract_file_from_tar_works() {
        // Build a small tar with a test file
        let mut buf = Vec::new();
        {
            let mut archive = tar::Builder::new(&mut buf);
            let content = b"hello world";
            let mut header = tar::Header::new_gnu();
            header.set_size(content.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            archive
                .append_data(&mut header, "some/path/myfile", &content[..])
                .unwrap();
            archive.finish().unwrap();
        }

        let result = extract_file_from_tar(&buf, "myfile").unwrap();
        assert_eq!(result, b"hello world");
    }

    #[test]
    fn extract_file_from_tar_missing_file() {
        let mut buf = Vec::new();
        {
            let mut archive = tar::Builder::new(&mut buf);
            let content = b"data";
            let mut header = tar::Header::new_gnu();
            header.set_size(content.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            archive
                .append_data(&mut header, "other", &content[..])
                .unwrap();
            archive.finish().unwrap();
        }

        let result = extract_file_from_tar(&buf, "missing");
        assert!(result.is_err());
    }

    #[test]
    fn verify_checksum_match_passes() {
        let data = b"hello world";
        let expected = hex::encode(Sha256::digest(data));
        assert!(verify_agent_checksum(data, &expected).is_ok());
    }

    #[test]
    fn verify_checksum_mismatch_fails() {
        let data = b"hello world";
        let wrong_hash = "0000000000000000000000000000000000000000000000000000000000000000";
        let result = verify_agent_checksum(data, wrong_hash);
        assert!(matches!(
            result,
            Err(CellaDockerError::AgentChecksumMismatch { .. })
        ));
    }

    #[test]
    fn parse_sha256sums_finds_artifact() {
        let contents = "abc123  cella-v0.1.0-x86_64-unknown-linux-musl.tar.gz\ndef456  cella-agent-x86_64\n789abc  cella-agent-aarch64\n";
        let result = parse_sha256sums(contents, "cella-agent-x86_64").unwrap();
        assert_eq!(result, "def456");
    }

    #[test]
    fn parse_sha256sums_missing_artifact() {
        let contents = "abc123  cella-agent-x86_64\n";
        let result = parse_sha256sums(contents, "cella-agent-aarch64");
        assert!(result.is_err());
    }
}
