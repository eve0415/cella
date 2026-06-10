//! CLI binary discovery for the Apple Container runtime.
//!
//! Searches for the `container` binary using environment variables and `PATH`,
//! then validates it by querying `container system version` and enforcing the
//! minimum supported release.

use std::env;
use std::fmt;
use std::path::{Path, PathBuf};

use tracing::{debug, warn};

use crate::sdk::ContainerCli;
use crate::sdk::run::run_cli;
use crate::sdk::types::VersionInfo;

/// Name of the environment variable that overrides binary lookup.
const ENV_BINARY_PATH: &str = "CELLA_CONTAINER_PATH";

/// Default binary name to search for in `PATH`.
const BINARY_NAME: &str = "container";

/// Minimum supported Apple Container release.
///
/// 1.0.0 stabilized the CLI surface and the structured-output shapes the SDK
/// parses (`container ls/inspect`, `image inspect`, `network`, `volume`).
/// Earlier releases emit incompatible JSON and lack `container cp`.
pub const MIN_SUPPORTED_VERSION: &str = "1.0.0";

/// Why discovery did not produce a usable CLI handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveryError {
    /// No `container` binary found via `CELLA_CONTAINER_PATH` or `PATH`.
    BinaryNotFound,
    /// A binary was found but did not behave like Apple's container CLI.
    NotAppleContainer {
        /// Path of the rejected binary.
        path: PathBuf,
        /// Human-readable reason for the rejection.
        reason: String,
    },
    /// Apple's CLI was found but is older than [`MIN_SUPPORTED_VERSION`].
    UnsupportedVersion {
        /// Path of the outdated binary.
        path: PathBuf,
        /// Version string reported by the binary.
        found: String,
    },
}

impl fmt::Display for DiscoveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BinaryNotFound => write!(
                f,
                "Apple Container CLI not found. \
                 Install from https://github.com/apple/container"
            ),
            Self::NotAppleContainer { path, reason } => write!(
                f,
                "binary at {} is not the Apple Container CLI: {reason}",
                path.display()
            ),
            Self::UnsupportedVersion { path, found } => write!(
                f,
                "Apple Container CLI at {} is version {found}, but cella requires \
                 {MIN_SUPPORTED_VERSION} or newer. \
                 Upgrade from https://github.com/apple/container/releases",
                path.display()
            ),
        }
    }
}

impl std::error::Error for DiscoveryError {}

/// Discover the Apple Container CLI binary.
///
/// Strategy:
/// 1. Check `CELLA_CONTAINER_PATH` environment variable
/// 2. Search for `container` in `PATH`
/// 3. Run `container system version --format json` to validate it is Apple's
///    tool and enforce [`MIN_SUPPORTED_VERSION`]
///
/// # Errors
///
/// Returns [`DiscoveryError`] describing whether the binary is missing,
/// foreign, or too old.
pub fn discover() -> Result<ContainerCli, DiscoveryError> {
    let binary_path = find_binary().ok_or(DiscoveryError::BinaryNotFound)?;
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
        let rt = tokio::runtime::Runtime::new().map_err(|e| DiscoveryError::NotAppleContainer {
            path: binary_path.clone(),
            reason: format!("failed to create validation runtime: {e}"),
        })?;
        rt.block_on(validate_binary(&binary_path))
    };

    match version_result {
        Ok(version) => {
            debug!(version, "validated Apple Container CLI");
            Ok(ContainerCli::new(binary_path, version))
        }
        Err(e) => {
            debug!(error = %e, "binary at path is not a usable Apple Container CLI");
            Err(e)
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

/// Validate that the binary is Apple's Container CLI running a supported
/// release, and return the version string.
async fn validate_binary(binary: &Path) -> Result<String, DiscoveryError> {
    match query_version(binary).await {
        Ok(version) => {
            if version_supported(&version) {
                Ok(version)
            } else {
                Err(DiscoveryError::UnsupportedVersion {
                    path: binary.to_path_buf(),
                    found: version,
                })
            }
        }
        Err(reason) => {
            debug!(error = %reason, "system version probe failed, classifying binary");
            // Releases before 1.0.0 may not support `system version --format
            // json`. `system status` distinguishes an old Apple CLI (reject as
            // outdated) from a foreign binary (reject as not Apple's tool).
            if is_apple_container_via_system_status(binary).await {
                Err(DiscoveryError::UnsupportedVersion {
                    path: binary.to_path_buf(),
                    found: "unknown (pre-1.0)".to_string(),
                })
            } else {
                Err(DiscoveryError::NotAppleContainer {
                    path: binary.to_path_buf(),
                    reason,
                })
            }
        }
    }
}

/// Query `container system version --format json` for the CLI version.
async fn query_version(binary: &Path) -> Result<String, String> {
    let output = run_cli(binary, &["system", "version", "--format", "json"])
        .await
        .map_err(|e| format!("failed to run system version: {e}"))?;

    if output.exit_code != 0 {
        return Err(format!(
            "system version exited with code {}",
            output.exit_code
        ));
    }

    // Parse as array of VersionInfo (CLI entry plus, when the API server is
    // running, a server entry).
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
    if let Some(version) = entries.first().and_then(|e| e.version.clone()) {
        return Ok(version);
    }

    Err("no recognizable Apple Container version entry found".to_string())
}

/// Probe `container system status` to tell an old Apple Container CLI apart
/// from an unrelated binary that happens to be named `container`.
async fn is_apple_container_via_system_status(binary: &Path) -> bool {
    let Ok(output) = run_cli(binary, &["system", "status"]).await else {
        return false;
    };

    // Reject if the plugin itself is missing.
    if output.stderr.contains("Plugin") && output.stderr.contains("not found") {
        return false;
    }

    // A working Apple Container CLI returns exit 0 for "running" or outputs
    // status info even when the service is stopped.
    output.exit_code == 0 || !output.stdout.trim().is_empty()
}

/// Check whether a reported version satisfies [`MIN_SUPPORTED_VERSION`].
///
/// Fails open: a version string that cannot be parsed is accepted with a
/// warning, so a future format change in Apple's version output cannot lock
/// users out. The gate exists to reject *known-old* releases.
fn version_supported(version: &str) -> bool {
    let Some(found) = parse_version(version) else {
        warn!(
            version,
            "could not parse Apple Container version; assuming supported"
        );
        return true;
    };
    // MIN_SUPPORTED_VERSION is a valid literal; parse cannot fail.
    let minimum = parse_version(MIN_SUPPORTED_VERSION).unwrap_or((1, 0, 0));
    found >= minimum
}

/// Parse a `major.minor.patch` version string into a comparable tuple.
///
/// Tolerates missing segments (`"1.2"` → `(1, 2, 0)`) and non-numeric
/// suffixes (`"1.0.0-beta.2"` → `(1, 0, 0)`). Returns `None` when the major
/// segment has no leading digits.
fn parse_version(version: &str) -> Option<(u64, u64, u64)> {
    let mut segments = version.trim().splitn(3, '.');
    let major = parse_segment(segments.next()?)?;
    let minor = segments.next().and_then(parse_segment).unwrap_or(0);
    let patch = segments.next().and_then(parse_segment).unwrap_or(0);
    Some((major, minor, patch))
}

/// Parse the leading decimal digits of a version segment.
fn parse_segment(segment: &str) -> Option<u64> {
    let digits: &str = segment
        .find(|c: char| !c.is_ascii_digit())
        .map_or(segment, |end| &segment[..end]);
    digits.parse().ok()
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
        old_version: PathBuf,
        fail: PathBuf,
        invalid_json: PathBuf,
        empty_array: PathBuf,
        unknown_app: PathBuf,
        no_version: PathBuf,
        pre_one_zero: PathBuf,
    }

    fn disc_mocks() -> &'static DiscMocks {
        use std::sync::OnceLock;

        static MOCKS: OnceLock<DiscMocks> = OnceLock::new();
        MOCKS.get_or_init(|| {
            let dir = tempfile::TempDir::new().unwrap();

            let write_script = |name: &str, body: &str| {
                crate::test_support::write_mock_script(dir.path(), name, body)
            };

            DiscMocks {
                valid_version: write_script(
                    "valid_version.sh",
                    r#"echo '[{"version":"2.0.0","appName":"container"}]'"#,
                ),
                old_version: write_script(
                    "old_version.sh",
                    r#"echo '[{"version":"0.12.3","appName":"container"}]'"#,
                ),
                fail: write_script("fail.sh", "exit 1"),
                invalid_json: write_script("invalid_json.sh", "echo 'not json'"),
                empty_array: write_script("empty_array.sh", "echo '[]'"),
                unknown_app: write_script(
                    "unknown_app.sh",
                    r#"echo '[{"version":"3.0.0","appName":"some-other-tool"}]'"#,
                ),
                no_version: write_script("no_version.sh", "echo '[{}]'"),
                // Pre-1.0 behavior: `system version` is unavailable but
                // `system status` works.
                pre_one_zero: write_script(
                    "pre_one_zero.sh",
                    r#"case "$1 $2" in
"system version") echo 'Error: unknown subcommand' >&2; exit 1;;
"system status") echo 'apiserver is running'; exit 0;;
*) exit 1;;
esac"#,
                ),
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

    // -- parse_version / version_supported -------------------------------------

    #[test]
    fn parse_version_full() {
        assert_eq!(parse_version("1.2.3"), Some((1, 2, 3)));
    }

    #[test]
    fn parse_version_short() {
        assert_eq!(parse_version("1.2"), Some((1, 2, 0)));
        assert_eq!(parse_version("2"), Some((2, 0, 0)));
    }

    #[test]
    fn parse_version_prerelease_suffix() {
        assert_eq!(parse_version("1.0.0-beta.2"), Some((1, 0, 0)));
        assert_eq!(parse_version("1.0.1+build5"), Some((1, 0, 1)));
    }

    #[test]
    fn parse_version_garbage() {
        assert_eq!(parse_version("unknown"), None);
        assert_eq!(parse_version(""), None);
    }

    #[test]
    fn version_supported_accepts_minimum_and_newer() {
        assert!(version_supported("1.0.0"));
        assert!(version_supported("1.0.1"));
        assert!(version_supported("1.2.0"));
        assert!(version_supported("2.0.0"));
    }

    #[test]
    fn version_supported_rejects_pre_one_zero() {
        assert!(!version_supported("0.12.3"));
        assert!(!version_supported("0.9.0"));
    }

    #[test]
    fn version_supported_fails_open_on_unparseable() {
        assert!(version_supported("unknown"));
    }

    // -- validate_binary tests ------------------------------------------------

    #[tokio::test]
    async fn query_version_with_valid_json() {
        let result = query_version(&disc_mocks().valid_version).await;
        assert_eq!(result.unwrap(), "2.0.0");
    }

    #[tokio::test]
    async fn query_version_with_nonzero_exit() {
        let result = query_version(&disc_mocks().fail).await;
        assert!(result.unwrap_err().contains("exited with code"));
    }

    #[tokio::test]
    async fn query_version_with_invalid_json() {
        let result = query_version(&disc_mocks().invalid_json).await;
        assert!(result.unwrap_err().contains("parse version JSON"));
    }

    #[tokio::test]
    async fn query_version_empty_array() {
        let result = query_version(&disc_mocks().empty_array).await;
        assert!(result.unwrap_err().contains("no recognizable"));
    }

    #[tokio::test]
    async fn query_version_unknown_app_name_with_version() {
        let result = query_version(&disc_mocks().unknown_app).await;
        assert_eq!(result.unwrap(), "3.0.0");
    }

    #[tokio::test]
    async fn query_version_no_version_no_app_name() {
        let result = query_version(&disc_mocks().no_version).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn validate_binary_nonexistent_path() {
        let result = validate_binary(Path::new("/nonexistent/binary")).await;
        assert!(matches!(
            result,
            Err(DiscoveryError::NotAppleContainer { .. })
        ));
    }

    #[tokio::test]
    async fn validate_binary_rejects_old_release() {
        let result = validate_binary(&disc_mocks().old_version).await;
        match result {
            Err(DiscoveryError::UnsupportedVersion { found, .. }) => {
                assert_eq!(found, "0.12.3");
            }
            other => panic!("expected UnsupportedVersion, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn validate_binary_classifies_pre_one_zero_as_unsupported() {
        let result = validate_binary(&disc_mocks().pre_one_zero).await;
        match result {
            Err(DiscoveryError::UnsupportedVersion { found, .. }) => {
                assert!(found.contains("pre-1.0"), "found: {found}");
            }
            other => panic!("expected UnsupportedVersion, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn validate_binary_rejects_foreign_binary() {
        // fail.sh exits 1 for every subcommand: neither the version probe nor
        // the system status probe recognizes it.
        let result = validate_binary(&disc_mocks().fail).await;
        assert!(matches!(
            result,
            Err(DiscoveryError::NotAppleContainer { .. })
        ));
    }

    #[tokio::test]
    async fn validate_binary_accepts_supported_release() {
        let result = validate_binary(&disc_mocks().valid_version).await;
        assert_eq!(result.unwrap(), "2.0.0");
    }

    // -- DiscoveryError display -------------------------------------------------

    #[test]
    fn discovery_error_not_found_mentions_install() {
        let msg = DiscoveryError::BinaryNotFound.to_string();
        assert!(msg.contains("github.com/apple/container"));
    }

    #[test]
    fn discovery_error_unsupported_mentions_minimum() {
        let msg = DiscoveryError::UnsupportedVersion {
            path: PathBuf::from("/usr/local/bin/container"),
            found: "0.12.3".to_string(),
        }
        .to_string();
        assert!(msg.contains("0.12.3"));
        assert!(msg.contains(MIN_SUPPORTED_VERSION));
    }
}
