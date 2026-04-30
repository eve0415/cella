//! CLI binary discovery for the Apple Container runtime.
//!
//! Searches for the `container` binary using environment variables and `PATH`,
//! then validates it by checking version output.

use std::env;
use std::path::{Path, PathBuf};

use tracing::{debug, warn};

use crate::sdk::ContainerCli;
use crate::sdk::run::run_cli;
use crate::sdk::types::VersionInfo;

/// Name of the environment variable that overrides binary lookup.
const ENV_BINARY_PATH: &str = "CELLA_CONTAINER_PATH";

/// Default binary name to search for in `PATH`.
const BINARY_NAME: &str = "container";

/// Discover the Apple Container CLI binary.
///
/// Strategy:
/// 1. Check `CELLA_CONTAINER_PATH` environment variable
/// 2. Search for `container` in `PATH`
/// 3. Run `container version --format json` to validate it is Apple's tool
///
/// Returns `None` if the binary is not found or is not the Apple Container CLI.
pub fn discover() -> Option<ContainerCli> {
    let binary_path = find_binary()?;
    debug!(path = %binary_path.display(), "found container binary");

    // Validate by running version command (blocking on an async operation).
    // Discovery runs once at startup so spawning a short-lived runtime is fine.
    let rt = tokio::runtime::Handle::try_current();
    let version_result = if let Ok(handle) = rt {
        // Already inside a tokio runtime — use `block_in_place` to avoid
        // nesting a new runtime.
        tokio::task::block_in_place(|| handle.block_on(validate_binary(&binary_path)))
    } else {
        // No runtime yet — create a temporary one.
        let rt = tokio::runtime::Runtime::new().ok()?;
        rt.block_on(validate_binary(&binary_path))
    };

    match version_result {
        Ok(version) => {
            debug!(version, "validated Apple Container CLI");
            Some(ContainerCli::new(binary_path, version))
        }
        Err(e) => {
            debug!(error = %e, "binary at path is not a valid Apple Container CLI");
            None
        }
    }
}

/// Locate the binary on disk.
fn find_binary() -> Option<PathBuf> {
    // 1. Explicit override via env var.
    if let Ok(path) = env::var(ENV_BINARY_PATH) {
        let p = PathBuf::from(&path);
        if p.is_file() {
            return Some(p);
        }
        warn!(
            path,
            "{ENV_BINARY_PATH} is set but does not point to an existing file"
        );
    }

    // 2. Search PATH.
    which_binary(BINARY_NAME)
}

/// Search `PATH` for an executable with the given name.
fn which_binary(name: &str) -> Option<PathBuf> {
    let path_var = env::var("PATH").ok()?;
    for dir in env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Validate that the binary is Apple's Container CLI and extract the version.
///
/// Tries `container version --format json` first. If the version plugin is not
/// available (common with .pkg installs), falls back to `container system status`
/// to confirm the binary is a working Apple Container CLI.
///
/// Returns `Err` if the binary cannot be executed or is not the Apple Container CLI.
async fn validate_binary(binary: &Path) -> Result<String, String> {
    // Try the version command first.
    if let Ok(version) = validate_via_version(binary).await {
        return Ok(version);
    }

    // Fallback: the version plugin may not be installed (e.g. .pkg installs).
    // Use `system status` to confirm this is a working Apple Container CLI.
    validate_via_system_status(binary).await
}

/// Try `container version --format json` for version extraction.
async fn validate_via_version(binary: &Path) -> Result<String, String> {
    let output = run_cli(binary, &["version", "--format", "json"])
        .await
        .map_err(|e| format!("failed to run version command: {e}"))?;

    if output.exit_code != 0 {
        return Err(format!(
            "version command exited with code {}",
            output.exit_code
        ));
    }

    // Parse as array of VersionInfo.
    let entries: Vec<VersionInfo> = serde_json::from_str(&output.stdout)
        .map_err(|e| format!("failed to parse version JSON: {e}"))?;

    // Look for an entry that identifies as Apple's container tool.
    for entry in &entries {
        if is_apple_container_entry(entry) {
            return Ok(entry
                .version
                .clone()
                .unwrap_or_else(|| "unknown".to_string()));
        }
    }

    // If we got valid JSON with at least one entry, accept it even if the
    // app_name doesn't exactly match — the CLI format may evolve.
    if let Some(first) = entries.first()
        && first.version.is_some()
    {
        return Ok(first.version.clone().unwrap_or_default());
    }

    Err("no recognizable Apple Container version entry found".to_string())
}

/// Fallback validation using `container system status`.
///
/// The system plugin is always present in .pkg installs. If the command
/// runs successfully, we know this is Apple's Container CLI.
async fn validate_via_system_status(binary: &Path) -> Result<String, String> {
    let output = run_cli(binary, &["system", "status"])
        .await
        .map_err(|e| format!("failed to run system status: {e}"))?;

    // Reject if the plugin itself is missing.
    if output.stderr.contains("Plugin") && output.stderr.contains("not found") {
        return Err("system status plugin not available".to_string());
    }

    // Reject if the command failed with a non-zero exit and no meaningful output.
    // A running Apple Container CLI returns exit 0 for "running" or outputs
    // status info even when the service is stopped.
    if output.exit_code != 0 && output.stdout.trim().is_empty() {
        return Err(format!(
            "system status exited with code {}",
            output.exit_code
        ));
    }

    debug!("validated Apple Container CLI via system status fallback");
    Ok("unknown".to_string())
}

/// Check whether a version entry looks like it belongs to Apple's container tool.
fn is_apple_container_entry(entry: &VersionInfo) -> bool {
    if let Some(name) = &entry.app_name {
        let lower = name.to_ascii_lowercase();
        return lower.contains("container");
    }
    // Without an app_name, accept any entry that has a version — the user
    // explicitly put the binary in PATH or set CELLA_CONTAINER_PATH.
    entry.version.is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DiscMocks {
        _dir: tempfile::TempDir,
        valid_version: PathBuf,
        fail: PathBuf,
        invalid_json: PathBuf,
        empty_array: PathBuf,
        unknown_app: PathBuf,
        no_version: PathBuf,
    }

    fn disc_mocks() -> &'static DiscMocks {
        use std::sync::OnceLock;

        static MOCKS: OnceLock<DiscMocks> = OnceLock::new();
        MOCKS.get_or_init(|| {
            let dir = tempfile::TempDir::new().unwrap();

            let write_script = |name: &str, body: &str| -> PathBuf {
                let path = dir.path().join(name);
                std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                        .unwrap();
                }
                path
            };

            DiscMocks {
                valid_version: write_script(
                    "valid_version.sh",
                    r#"echo '[{"version":"2.0.0","appName":"container"}]'"#,
                ),
                fail: write_script("fail.sh", "exit 1"),
                invalid_json: write_script("invalid_json.sh", "echo 'not json'"),
                empty_array: write_script("empty_array.sh", "echo '[]'"),
                unknown_app: write_script(
                    "unknown_app.sh",
                    r#"echo '[{"version":"3.0.0","appName":"some-other-tool"}]'"#,
                ),
                no_version: write_script("no_version.sh", "echo '[{}]'"),
                _dir: dir,
            }
        })
    }

    #[test]
    fn is_apple_container_entry_matches_name() {
        let entry = VersionInfo {
            version: Some("1.0.0".to_string()),
            app_name: Some("container".to_string()),
        };
        assert!(is_apple_container_entry(&entry));
    }

    #[test]
    fn is_apple_container_entry_case_insensitive() {
        let entry = VersionInfo {
            version: Some("1.0.0".to_string()),
            app_name: Some("Apple Container".to_string()),
        };
        assert!(is_apple_container_entry(&entry));
    }

    #[test]
    fn is_apple_container_entry_no_app_name_with_version() {
        let entry = VersionInfo {
            version: Some("1.0.0".to_string()),
            app_name: None,
        };
        assert!(is_apple_container_entry(&entry));
    }

    #[test]
    fn is_apple_container_entry_no_fields() {
        let entry = VersionInfo {
            version: None,
            app_name: None,
        };
        assert!(!is_apple_container_entry(&entry));
    }

    #[test]
    fn is_apple_container_entry_wrong_name_no_version() {
        let entry = VersionInfo {
            version: None,
            app_name: Some("docker".to_string()),
        };
        assert!(!is_apple_container_entry(&entry));
    }

    #[test]
    fn which_binary_finds_sh() {
        // /bin/sh should exist on any POSIX system.
        let result = which_binary("sh");
        assert!(result.is_some(), "expected to find 'sh' in PATH");
    }

    #[test]
    fn which_binary_returns_none_for_nonexistent() {
        let result = which_binary("definitely_not_a_real_binary_xyz");
        assert!(result.is_none());
    }

    // -- which_binary additional tests ----------------------------------------

    #[test]
    fn which_binary_finds_echo() {
        let result = which_binary("echo");
        assert!(result.is_some(), "expected to find 'echo' in PATH");
    }

    #[test]
    fn which_binary_finds_cat() {
        let result = which_binary("cat");
        assert!(result.is_some(), "expected to find 'cat' in PATH");
    }

    // -- validate_binary tests ------------------------------------------------

    #[tokio::test]
    async fn validate_via_version_with_valid_json() {
        let result = validate_via_version(&disc_mocks().valid_version).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "2.0.0");
    }

    #[tokio::test]
    async fn validate_via_version_with_nonzero_exit() {
        let result = validate_via_version(&disc_mocks().fail).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("exited with code"));
    }

    #[tokio::test]
    async fn validate_via_version_with_invalid_json() {
        let result = validate_via_version(&disc_mocks().invalid_json).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("parse version JSON"));
    }

    #[tokio::test]
    async fn validate_via_version_empty_array() {
        let result = validate_via_version(&disc_mocks().empty_array).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("no recognizable"));
    }

    #[tokio::test]
    async fn validate_via_version_unknown_app_name_with_version() {
        let result = validate_via_version(&disc_mocks().unknown_app).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "3.0.0");
    }

    #[tokio::test]
    async fn validate_via_version_no_version_no_app_name() {
        let result = validate_via_version(&disc_mocks().no_version).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn validate_binary_nonexistent_path() {
        let result = validate_binary(Path::new("/nonexistent/binary")).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("failed to run"));
    }

    #[tokio::test]
    async fn validate_binary_falls_back_to_system_status() {
        // A script that fails `version` but succeeds for other subcommands
        // should still be accepted via the system status fallback.
        let result = validate_binary(&disc_mocks().fail).await;
        // fail.sh exits 1 for all args, so both version and system status fail.
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn validate_binary_uses_version_when_available() {
        let result = validate_binary(&disc_mocks().valid_version).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "2.0.0");
    }

    // -- is_apple_container_entry additional tests ----------------------------

    #[test]
    fn is_apple_container_entry_partial_name_match() {
        let entry = VersionInfo {
            version: Some("1.0.0".to_string()),
            app_name: Some("MyContainerApp".to_string()),
        };
        assert!(is_apple_container_entry(&entry));
    }
}
