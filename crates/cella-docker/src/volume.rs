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

/// Stable symlink path for the agent binary inside the container.
///
/// Points to the latest versioned agent binary. Survives version upgrades
/// so containers with hardcoded CMD paths continue to work.
pub fn agent_symlink_path() -> String {
    format!("{AGENT_PATH_PREFIX}/bin/cella-agent")
}

/// Get the CLI symlink path inside the container.
///
/// This symlink points to the agent binary. When invoked via this path,
/// the agent enters CLI mode for in-container worktree commands.
pub fn cli_symlink_path() -> String {
    format!("{AGENT_PATH_PREFIX}/bin/cella")
}

/// Get the credential helper path inside the container.
///
/// Uses the stable agent symlink with the `credential` subcommand.
pub fn credential_helper_path() -> String {
    format!("{AGENT_PATH_PREFIX}/bin/cella-agent credential")
}

/// Path for the daemon address file on the shared agent volume.
///
/// Contains two lines: `host:port` and auth token. Read by agents to
/// discover the daemon, enabling self-healing on daemon restarts.
pub const fn daemon_addr_file_path() -> &'static str {
    "/cella/.daemon_addr"
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
///
/// Uses the stable symlink so the script survives version upgrades.
pub fn browser_helper_script() -> Vec<u8> {
    let agent_path = agent_symlink_path();
    let script = format!(
        r#"#!/bin/sh
# cella browser helper — forwards URLs to host via cella-agent.
exec "{agent_path}" browser-open "$1"
"#
    );
    script.into_bytes()
}

/// Generate the xsel shim script content.
///
/// Placed at `/cella/bin/xsel` to shadow the real xsel binary.
/// Forwards all arguments to the agent's clipboard handler.
pub fn xsel_helper_script() -> Vec<u8> {
    let agent_path = agent_symlink_path();
    format!(
        r#"#!/bin/sh
exec "{agent_path}" xsel "$@"
"#
    )
    .into_bytes()
}

/// Generate the xclip shim script content.
///
/// Placed at `/cella/bin/xclip` to shadow the real xclip binary.
/// Forwards all arguments to the agent's clipboard handler.
pub fn xclip_helper_script() -> Vec<u8> {
    let agent_path = agent_symlink_path();
    format!(
        r#"#!/bin/sh
exec "{agent_path}" xclip "$@"
"#
    )
    .into_bytes()
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
    let browser_script = browser_helper_script();
    let xsel_script = xsel_helper_script();
    let xclip_script = xclip_helper_script();

    upload_to_volume(
        docker,
        version,
        arch,
        &agent_bytes,
        &browser_script,
        &xsel_script,
        &xclip_script,
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
    xsel_script: &[u8],
    xclip_script: &[u8],
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
    let tar_bytes = build_volume_tar(
        version,
        arch,
        agent_bytes,
        browser_script,
        xsel_script,
        xclip_script,
        version_marker,
    )?;

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
    xsel_script: &[u8],
    xclip_script: &[u8],
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

        // xsel shim: /cella/bin/xsel
        let mut header = tar::Header::new_gnu();
        header.set_size(xsel_script.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        archive
            .append_data(&mut header, "cella/bin/xsel", xsel_script)
            .map_err(|e| CellaDockerError::AgentVolume {
                message: format!("tar append xsel: {e}"),
            })?;

        // xclip shim: /cella/bin/xclip
        let mut header = tar::Header::new_gnu();
        header.set_size(xclip_script.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        archive
            .append_data(&mut header, "cella/bin/xclip", xclip_script)
            .map_err(|e| CellaDockerError::AgentVolume {
                message: format!("tar append xclip: {e}"),
            })?;

        // Stable agent symlink: /cella/bin/cella-agent -> versioned binary
        // Survives version upgrades so CMD paths and credential helpers keep working.
        let agent_link_target = format!("/cella/v{version}/{arch}/cella-agent");
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        header.set_mode(0o755);
        header.set_cksum();
        archive
            .append_link(&mut header, "cella/bin/cella-agent", &agent_link_target)
            .map_err(|e| CellaDockerError::AgentVolume {
                message: format!("tar append cella-agent symlink: {e}"),
            })?;

        // CLI symlink: /cella/bin/cella -> stable agent symlink
        // When invoked as "cella", the agent enters CLI mode for in-container commands.
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        header.set_mode(0o755);
        header.set_cksum();
        archive
            .append_link(&mut header, "cella/bin/cella", "/cella/bin/cella-agent")
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

/// Write the daemon address file to the agent volume.
///
/// The file contains two lines: the daemon address (`host:port`) and the
/// auth token. Agents read this file on startup and reconnect to discover
/// the current daemon, enabling self-healing after daemon restarts.
///
/// # Errors
///
/// Returns error if volume access or file upload fails.
pub async fn write_daemon_addr_file(
    docker: &Docker,
    daemon_addr: &str,
    daemon_token: &str,
) -> Result<(), CellaDockerError> {
    ensure_image_pulled(docker, "alpine:3").await?;

    let container_name = "cella-daemon-addr-write";

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
        cmd: Some(vec!["sleep".to_string(), "10".to_string()]),
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

    // Build a tar with just the .daemon_addr file
    let content = format!("{daemon_addr}\n{daemon_token}\n");
    let content_bytes = content.as_bytes();
    let mut buf = Vec::new();
    {
        let mut archive = tar::Builder::new(&mut buf);
        let mut header = tar::Header::new_gnu();
        header.set_size(content_bytes.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        archive
            .append_data(&mut header, "cella/.daemon_addr", content_bytes)
            .map_err(|e| CellaDockerError::AgentVolume {
                message: format!("tar append .daemon_addr: {e}"),
            })?;
        archive
            .finish()
            .map_err(|e| CellaDockerError::AgentVolume {
                message: format!("tar finish: {e}"),
            })?;
    }

    docker
        .upload_to_container(
            container_name,
            Some(bollard::query_parameters::UploadToContainerOptions {
                path: "/".to_string(),
                ..Default::default()
            }),
            bollard::body_full(buf.into()),
        )
        .await?;

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

    debug!("Wrote .daemon_addr file to agent volume");
    Ok(())
}

/// Prune old agent versions from the volume, keeping the current version
/// and any versions used by running containers.
///
/// # Errors
///
/// Returns error if Docker API calls fail.
pub async fn prune_old_agent_versions(
    docker: &Docker,
    current_version: &str,
) -> Result<(), CellaDockerError> {
    ensure_image_pulled(docker, "alpine:3").await?;

    // Find versions used by running cella containers
    let mut versions_in_use = std::collections::HashSet::new();
    versions_in_use.insert(current_version.to_string());

    let filters: std::collections::HashMap<String, Vec<String>> = [
        (
            "label".to_string(),
            vec!["dev.cella.tool=cella".to_string()],
        ),
        ("status".to_string(), vec!["running".to_string()]),
    ]
    .into_iter()
    .collect();
    if let Ok(containers) = docker
        .list_containers(Some(bollard::query_parameters::ListContainersOptions {
            filters: Some(filters),
            ..Default::default()
        }))
        .await
    {
        for c in &containers {
            if let Some(labels) = &c.labels
                && let Some(ver) = labels.get("dev.cella.version")
            {
                versions_in_use.insert(ver.clone());
            }
        }
    }

    let versions_on_volume = list_volume_versions(docker).await?;

    let to_delete: Vec<&String> = versions_on_volume
        .iter()
        .filter(|v| !versions_in_use.contains(v.as_str()))
        .collect();

    if to_delete.is_empty() {
        return Ok(());
    }

    delete_version_dirs(docker, &to_delete).await
}

/// List agent version directories present on the volume.
async fn list_volume_versions(docker: &Docker) -> Result<Vec<String>, CellaDockerError> {
    let container_name = "cella-version-prune";
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
        cmd: Some(vec![
            "sh".to_string(),
            "-c".to_string(),
            "ls -d /cella/v*/ 2>/dev/null | sed 's|/cella/v||;s|/||g'".to_string(),
        ]),
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

    let mut wait_stream =
        docker.wait_container(container_name, Some(WaitContainerOptions::default()));
    while let Some(result) = wait_stream.next().await {
        if result.is_err() {
            break;
        }
    }

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

    Ok(output
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect())
}

/// Delete specific version directories from the agent volume.
async fn delete_version_dirs(
    docker: &Docker,
    versions: &[&String],
) -> Result<(), CellaDockerError> {
    let rm_cmds: Vec<String> = versions
        .iter()
        .map(|v| format!("rm -rf /cella/v{v}"))
        .collect();
    let rm_script = rm_cmds.join(" && ");

    let container_name = "cella-version-prune-rm";
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
        cmd: Some(vec!["sh".to_string(), "-c".to_string(), rm_script]),
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

    let mut wait_stream =
        docker.wait_container(container_name, Some(WaitContainerOptions::default()));
    while let Some(result) = wait_stream.next().await {
        if result.is_err() {
            break;
        }
    }

    info!(
        "Pruned old agent versions: {}",
        versions
            .iter()
            .map(|v| format!("v{v}"))
            .collect::<Vec<_>>()
            .join(", ")
    );

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
    fn agent_symlink_path_format() {
        assert_eq!(agent_symlink_path(), "/cella/bin/cella-agent");
    }

    #[test]
    fn browser_script_uses_stable_symlink() {
        let script = browser_helper_script();
        let content = String::from_utf8(script).unwrap();
        assert!(content.contains("/cella/bin/cella-agent"));
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

        let tar_bytes = build_volume_tar(
            "0.1.0",
            "aarch64",
            agent_bytes,
            browser_bytes,
            b"s",
            b"s",
            marker,
        )
        .unwrap();

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
        assert!(entries.iter().any(|e| e == "cella/bin/cella"));
        assert!(
            entries.iter().any(|e| e == "cella/bin/xsel"),
            "tar must contain xsel shim"
        );
        assert!(
            entries.iter().any(|e| e == "cella/bin/xclip"),
            "tar must contain xclip shim"
        );
    }

    #[test]
    fn cli_symlink_path_format() {
        assert_eq!(cli_symlink_path(), "/cella/bin/cella");
    }

    #[test]
    fn credential_helper_path_format() {
        assert_eq!(
            credential_helper_path(),
            "/cella/bin/cella-agent credential"
        );
    }

    #[test]
    fn build_volume_tar_agent_symlink_points_to_versioned() {
        let agent_bytes = b"fake-binary";
        let browser_bytes = b"#!/bin/sh";
        let marker = "0.1.0/x86_64\n";

        let tar_bytes = build_volume_tar(
            "0.1.0",
            "x86_64",
            agent_bytes,
            browser_bytes,
            b"s",
            b"s",
            marker,
        )
        .unwrap();

        let mut archive = tar::Archive::new(tar_bytes.as_slice());
        let mut found_agent_symlink = false;
        let mut found_cella_symlink = false;
        for entry in archive.entries().unwrap() {
            let entry = entry.unwrap();
            let path = entry.path().unwrap().to_string_lossy().to_string();
            if path == "cella/bin/cella-agent" {
                assert_eq!(entry.header().entry_type(), tar::EntryType::Symlink);
                let link = entry.link_name().unwrap().unwrap();
                assert_eq!(link.to_string_lossy(), "/cella/v0.1.0/x86_64/cella-agent");
                found_agent_symlink = true;
            }
            if path == "cella/bin/cella" {
                assert_eq!(entry.header().entry_type(), tar::EntryType::Symlink);
                let link = entry.link_name().unwrap().unwrap();
                assert_eq!(link.to_string_lossy(), "/cella/bin/cella-agent");
                found_cella_symlink = true;
            }
        }
        assert!(
            found_agent_symlink,
            "cella/bin/cella-agent symlink not found"
        );
        assert!(found_cella_symlink, "cella/bin/cella symlink not found");
    }

    #[test]
    fn build_volume_tar_agent_is_executable() {
        let agent_bytes = b"fake-binary";
        let browser_bytes = b"#!/bin/sh";
        let marker = "0.1.0/x86_64\n";

        let tar_bytes = build_volume_tar(
            "0.1.0",
            "x86_64",
            agent_bytes,
            browser_bytes,
            b"s",
            b"s",
            marker,
        )
        .unwrap();

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

    // -----------------------------------------------------------------------
    // Additional coverage tests
    // -----------------------------------------------------------------------

    #[test]
    fn agent_binary_path_with_aarch64() {
        assert_eq!(
            agent_binary_path("1.2.3", "aarch64"),
            "/cella/v1.2.3/aarch64/cella-agent"
        );
    }

    #[test]
    fn agent_binary_path_with_prerelease_version() {
        assert_eq!(
            agent_binary_path("0.0.1-alpha", "x86_64"),
            "/cella/v0.0.1-alpha/x86_64/cella-agent"
        );
    }

    #[test]
    fn browser_helper_script_starts_with_shebang() {
        let script = browser_helper_script();
        let content = String::from_utf8(script).unwrap();
        assert!(content.starts_with("#!/bin/sh"));
    }

    #[test]
    fn browser_helper_script_exec_agent_browser_open() {
        let script = browser_helper_script();
        let content = String::from_utf8(script).unwrap();
        assert!(content.contains("exec \"/cella/bin/cella-agent\" browser-open \"$1\""));
    }

    #[test]
    fn browser_helper_script_uses_symlink_path() {
        let script = browser_helper_script();
        let content = String::from_utf8(script).unwrap();
        assert!(content.contains(&agent_symlink_path()));
    }

    #[test]
    fn xsel_helper_script_starts_with_shebang() {
        let script = xsel_helper_script();
        assert!(String::from_utf8_lossy(&script).starts_with("#!/bin/sh"));
    }

    #[test]
    fn xsel_helper_script_execs_agent_xsel() {
        let bytes = xsel_helper_script();
        let script = String::from_utf8_lossy(&bytes);
        assert!(script.contains("xsel"));
        assert!(script.contains("cella-agent"));
    }

    #[test]
    fn xclip_helper_script_starts_with_shebang() {
        let bytes = xclip_helper_script();
        assert!(String::from_utf8_lossy(&bytes).starts_with("#!/bin/sh"));
    }

    #[test]
    fn xclip_helper_script_execs_agent_xclip() {
        let bytes = xclip_helper_script();
        let script = String::from_utf8_lossy(&bytes);
        assert!(script.contains("xclip"));
        assert!(script.contains("cella-agent"));
    }

    #[test]
    fn version_marker_path_is_under_cella() {
        let path = version_marker_path();
        assert_eq!(path, "/cella/.version");
    }

    #[test]
    fn version_marker_content_contains_arch_and_newline() {
        let content = version_marker_content("aarch64");
        assert!(content.contains("aarch64"));
        assert!(content.ends_with('\n'));
        // Format: {version}/{arch}\n
        let parts: Vec<&str> = content.trim().split('/').collect();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[1], "aarch64");
    }

    #[test]
    fn version_marker_content_version_matches_cargo_pkg() {
        let content = version_marker_content("x86_64");
        let version = env!("CARGO_PKG_VERSION");
        assert!(
            content.starts_with(version),
            "expected marker to start with '{version}', got: {content}"
        );
    }

    #[test]
    fn cli_symlink_path_is_under_bin() {
        let path = cli_symlink_path();
        assert!(path.starts_with("/cella/bin/"));
        assert!(path.ends_with("/cella"));
    }

    #[test]
    fn credential_helper_path_is_under_bin() {
        let path = credential_helper_path();
        assert!(path.starts_with("/cella/bin/"));
        assert!(path.contains("credential"));
    }

    #[test]
    fn agent_volume_mount_source_matches_constant() {
        let (source, target, _) = agent_volume_mount();
        assert_eq!(source, AGENT_VOLUME_NAME);
        assert_eq!(target, "/cella");
    }

    #[test]
    fn detect_agent_arch_returns_x86_64_or_aarch64() {
        let arch = detect_agent_arch();
        // In CI/test environments we expect a known architecture
        assert!(
            arch == "x86_64" || arch == "aarch64",
            "unexpected arch: {arch}"
        );
    }

    #[test]
    fn parse_sha256sums_single_space_separator() {
        // Some tools produce single-space instead of double-space
        let contents = "abc123 cella-agent-x86_64\n";
        let result = parse_sha256sums(contents, "cella-agent-x86_64").unwrap();
        assert_eq!(result, "abc123");
    }

    #[test]
    fn parse_sha256sums_empty_input() {
        let result = parse_sha256sums("", "cella-agent-x86_64");
        assert!(result.is_err());
    }

    #[test]
    fn parse_sha256sums_multiple_artifacts() {
        let contents = "\
abc111  artifact-a
abc222  artifact-b
abc333  artifact-c
";
        assert_eq!(parse_sha256sums(contents, "artifact-a").unwrap(), "abc111");
        assert_eq!(parse_sha256sums(contents, "artifact-b").unwrap(), "abc222");
        assert_eq!(parse_sha256sums(contents, "artifact-c").unwrap(), "abc333");
    }

    #[test]
    fn parse_sha256sums_skips_malformed_lines() {
        let contents = "nospacehere\nabc123  good-artifact\n";
        let result = parse_sha256sums(contents, "good-artifact").unwrap();
        assert_eq!(result, "abc123");
    }

    #[test]
    fn verify_checksum_empty_data() {
        let data = b"";
        let expected = hex::encode(Sha256::digest(data));
        assert!(verify_agent_checksum(data, &expected).is_ok());
    }

    #[test]
    fn build_volume_tar_version_marker_content() {
        let agent_bytes = b"agent";
        let browser_bytes = b"browser";
        let marker = "1.0.0/x86_64\n";

        let tar_bytes = build_volume_tar(
            "1.0.0",
            "x86_64",
            agent_bytes,
            browser_bytes,
            b"s",
            b"s",
            marker,
        )
        .unwrap();

        let mut archive = tar::Archive::new(tar_bytes.as_slice());
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            let path = entry.path().unwrap().to_string_lossy().to_string();
            if path.contains(".version") {
                let mut content = String::new();
                std::io::Read::read_to_string(&mut entry, &mut content).unwrap();
                assert_eq!(content, "1.0.0/x86_64\n");
                return;
            }
        }
        panic!(".version file not found in tar");
    }

    #[test]
    fn build_volume_tar_browser_script_is_executable() {
        let tar_bytes =
            build_volume_tar("1.0.0", "x86_64", b"agent", b"#!/bin/sh", b"s", b"s", "m").unwrap();

        let mut archive = tar::Archive::new(tar_bytes.as_slice());
        for entry in archive.entries().unwrap() {
            let entry = entry.unwrap();
            let path = entry.path().unwrap().to_string_lossy().to_string();
            if path.contains("cella-browser") {
                assert_eq!(entry.header().mode().unwrap(), 0o755);
                return;
            }
        }
        panic!("cella-browser not found in tar");
    }

    // -----------------------------------------------------------------------
    // daemon_addr_file_path tests
    // -----------------------------------------------------------------------

    #[test]
    fn daemon_addr_file_path_returns_expected() {
        assert_eq!(daemon_addr_file_path(), "/cella/.daemon_addr");
    }

    #[test]
    fn daemon_addr_file_path_starts_with_cella_prefix() {
        let path = daemon_addr_file_path();
        assert!(path.starts_with(AGENT_PATH_PREFIX));
    }

    // -----------------------------------------------------------------------
    // Constants tests
    // -----------------------------------------------------------------------

    #[test]
    fn agent_volume_name_constant() {
        assert_eq!(AGENT_VOLUME_NAME, "cella-agent");
    }

    #[test]
    fn agent_path_prefix_is_cella() {
        // agent_binary_path uses AGENT_PATH_PREFIX internally
        let path = agent_binary_path("1.0.0", "x86_64");
        assert!(path.starts_with("/cella/"));
    }

    // -----------------------------------------------------------------------
    // build_volume_tar comprehensive tests
    // -----------------------------------------------------------------------

    #[test]
    fn build_volume_tar_contains_exactly_seven_entries() {
        let tar_bytes = build_volume_tar(
            "2.0.0",
            "aarch64",
            b"binary",
            b"#!/bin/sh",
            b"s",
            b"s",
            "2.0.0/aarch64\n",
        )
        .unwrap();

        let mut archive = tar::Archive::new(tar_bytes.as_slice());
        let entries: Vec<String> = archive
            .entries()
            .unwrap()
            .filter_map(|e| {
                e.ok()
                    .and_then(|entry| entry.path().ok().map(|p| p.to_string_lossy().to_string()))
            })
            .collect();

        // agent binary, browser script, xsel shim, xclip shim, agent symlink, cella symlink, .version
        assert_eq!(
            entries.len(),
            7,
            "expected 7 entries in volume tar, got {}: {entries:?}",
            entries.len()
        );
    }

    #[test]
    fn build_volume_tar_agent_binary_has_correct_content() {
        let agent_content = b"this is the agent binary content";
        let tar_bytes = build_volume_tar(
            "1.0.0",
            "x86_64",
            agent_content,
            b"script",
            b"s",
            b"s",
            "marker",
        )
        .unwrap();

        let mut archive = tar::Archive::new(tar_bytes.as_slice());
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            let path = entry.path().unwrap().to_string_lossy().to_string();
            if path == "cella/v1.0.0/x86_64/cella-agent" {
                let mut content = Vec::new();
                std::io::Read::read_to_end(&mut entry, &mut content).unwrap();
                assert_eq!(content, agent_content);
                return;
            }
        }
        panic!("agent binary not found at expected path");
    }

    #[test]
    fn build_volume_tar_browser_script_has_correct_content() {
        let browser_content = b"#!/bin/sh\necho browser";
        let tar_bytes = build_volume_tar(
            "1.0.0",
            "x86_64",
            b"agent",
            browser_content,
            b"s",
            b"s",
            "marker",
        )
        .unwrap();

        let mut archive = tar::Archive::new(tar_bytes.as_slice());
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            let path = entry.path().unwrap().to_string_lossy().to_string();
            if path == "cella/bin/cella-browser" {
                let mut content = Vec::new();
                std::io::Read::read_to_end(&mut entry, &mut content).unwrap();
                assert_eq!(content, browser_content);
                return;
            }
        }
        panic!("browser script not found at expected path");
    }

    #[test]
    fn build_volume_tar_version_marker_mode_is_644() {
        let tar_bytes = build_volume_tar(
            "1.0.0",
            "x86_64",
            b"agent",
            b"script",
            b"s",
            b"s",
            "1.0.0/x86_64\n",
        )
        .unwrap();

        let mut archive = tar::Archive::new(tar_bytes.as_slice());
        for entry in archive.entries().unwrap() {
            let entry = entry.unwrap();
            let path = entry.path().unwrap().to_string_lossy().to_string();
            if path.contains(".version") {
                assert_eq!(
                    entry.header().mode().unwrap(),
                    0o644,
                    ".version should have mode 644"
                );
                return;
            }
        }
        panic!(".version not found");
    }

    #[test]
    fn build_volume_tar_with_empty_agent_bytes() {
        // Should still produce a valid archive even with zero-length agent binary
        let tar_bytes =
            build_volume_tar("1.0.0", "x86_64", b"", b"script", b"s", b"s", "marker").unwrap();

        let mut archive = tar::Archive::new(tar_bytes.as_slice());
        let entries: Vec<String> = archive
            .entries()
            .unwrap()
            .filter_map(|e| {
                e.ok()
                    .and_then(|entry| entry.path().ok().map(|p| p.to_string_lossy().to_string()))
            })
            .collect();
        assert_eq!(entries.len(), 7);
    }

    // -----------------------------------------------------------------------
    // extract_file_from_tar additional tests
    // -----------------------------------------------------------------------

    #[test]
    fn extract_file_from_tar_multiple_files_finds_correct_one() {
        let mut buf = Vec::new();
        {
            let mut archive = tar::Builder::new(&mut buf);
            for (name, content) in &[("file_a", b"aaa" as &[u8]), ("file_b", b"bbb")] {
                let mut header = tar::Header::new_gnu();
                header.set_size(content.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                archive.append_data(&mut header, name, *content).unwrap();
            }
            archive.finish().unwrap();
        }

        let result = extract_file_from_tar(&buf, "file_b").unwrap();
        assert_eq!(result, b"bbb");
    }

    #[test]
    fn extract_file_from_tar_matches_filename_not_full_path() {
        let mut buf = Vec::new();
        {
            let mut archive = tar::Builder::new(&mut buf);
            let content = b"found it";
            let mut header = tar::Header::new_gnu();
            header.set_size(content.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            archive
                .append_data(&mut header, "deep/nested/path/target-file", &content[..])
                .unwrap();
            archive.finish().unwrap();
        }

        let result = extract_file_from_tar(&buf, "target-file").unwrap();
        assert_eq!(result, b"found it");
    }

    // -----------------------------------------------------------------------
    // verify_agent_checksum additional tests
    // -----------------------------------------------------------------------

    #[test]
    fn verify_checksum_mismatch_contains_both_hashes() {
        let data = b"test data";
        let actual_hash = hex::encode(Sha256::digest(data));
        let wrong_hash = "0".repeat(64);
        let err = verify_agent_checksum(data, &wrong_hash).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains(&wrong_hash),
            "error should contain expected hash"
        );
        assert!(
            msg.contains(&actual_hash),
            "error should contain actual hash"
        );
    }

    // -----------------------------------------------------------------------
    // parse_sha256sums additional tests
    // -----------------------------------------------------------------------

    #[test]
    fn parse_sha256sums_with_tabs_as_separator() {
        let contents = "abc123\tcella-agent-x86_64\n";
        let result = parse_sha256sums(contents, "cella-agent-x86_64").unwrap();
        assert_eq!(result, "abc123");
    }

    #[test]
    fn parse_sha256sums_extra_whitespace_in_name() {
        // Name with leading whitespace (from double-space format) should be trimmed
        let contents = "abc123  cella-agent-x86_64\n";
        let result = parse_sha256sums(contents, "cella-agent-x86_64").unwrap();
        assert_eq!(result, "abc123");
    }

    #[test]
    fn parse_sha256sums_returns_first_match() {
        // If multiple lines match, the first one should win
        let contents = "first_hash  same-name\nsecond_hash  same-name\n";
        let result = parse_sha256sums(contents, "same-name").unwrap();
        assert_eq!(result, "first_hash");
    }

    // -----------------------------------------------------------------------
    // version_marker_content additional tests
    // -----------------------------------------------------------------------

    #[test]
    fn version_marker_content_different_arches() {
        let x86 = version_marker_content("x86_64");
        let arm = version_marker_content("aarch64");
        assert_ne!(x86, arm);
        assert!(x86.contains("x86_64"));
        assert!(arm.contains("aarch64"));
    }

    // -----------------------------------------------------------------------
    // agent_binary_path edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn agent_binary_path_empty_version() {
        let path = agent_binary_path("", "x86_64");
        assert_eq!(path, "/cella/v/x86_64/cella-agent");
    }

    #[test]
    fn agent_binary_path_empty_arch() {
        let path = agent_binary_path("1.0.0", "");
        assert_eq!(path, "/cella/v1.0.0//cella-agent");
    }

    // -----------------------------------------------------------------------
    // dev_agent_override tests
    // -----------------------------------------------------------------------

    #[test]
    fn dev_agent_override_returns_none_when_unset() {
        // Ensure the env var is not set (it shouldn't be in test)
        // If it is set, this test just confirms the function works
        let result = dev_agent_override();
        // Either None (not set) or Some(path) — both are valid
        if let Some(path) = &result {
            assert!(!path.is_empty());
        }
    }
}
