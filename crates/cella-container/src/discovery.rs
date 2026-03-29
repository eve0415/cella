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

/// Run `container version --format json` and extract the version string.
///
/// Returns `Err` if the binary cannot be executed or the output does not
/// look like Apple's Container CLI.
async fn validate_binary(binary: &Path) -> Result<String, String> {
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
}
