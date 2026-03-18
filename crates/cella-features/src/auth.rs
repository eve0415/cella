//! Docker credential resolution for OCI registry authentication.
//!
//! Reads `~/.docker/config.json` and resolves credentials through three
//! mechanisms (in order of precedence):
//!
//! 1. **Inline `auths`** -- base64-encoded `username:password` stored directly
//!    in the config file.
//! 2. **Per-registry credential helpers** (`credHelpers`) -- delegates to
//!    `docker-credential-<helper> get` for a specific registry.
//! 3. **Global credential store** (`credsStore`) -- delegates to
//!    `docker-credential-<helper> get` for all registries.

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::Deserialize;
use tracing::debug;

/// Credentials for authenticating with a Docker / OCI registry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DockerCredentials {
    pub username: Option<String>,
    pub password: Option<String>,
}

impl DockerCredentials {
    /// Returns `true` when neither username nor password is set.
    pub const fn is_empty(&self) -> bool {
        self.username.is_none() && self.password.is_none()
    }
}

// ---------------------------------------------------------------------------
// Docker config.json schema (subset)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
struct DockerConfig {
    #[serde(default)]
    auths: HashMap<String, AuthEntry>,

    #[serde(default, rename = "credHelpers")]
    cred_helpers: HashMap<String, String>,

    #[serde(default, rename = "credsStore")]
    creds_store: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct AuthEntry {
    auth: Option<String>,
}

/// JSON response from `docker-credential-<helper> get`.
#[derive(Debug, Deserialize)]
struct CredentialHelperResponse {
    #[serde(default, rename = "Username")]
    username: Option<String>,
    #[serde(default, rename = "Secret")]
    secret: Option<String>,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Resolve Docker credentials for the given registry.
///
/// Checks the Docker config at `~/.docker/config.json` in the following
/// order:
///
/// 1. Inline `auths[registry].auth` (base64 `username:password`)
/// 2. `credHelpers[registry]` -- per-registry credential helper binary
/// 3. `credsStore` -- global credential helper binary
///
/// Returns empty credentials if the config file is missing, the registry
/// is not found, or any resolution step fails.
pub fn resolve_credentials(registry: &str) -> DockerCredentials {
    resolve_credentials_from(registry, default_config_path())
}

/// Path to `~/.docker/config.json`, or `None` if the home directory
/// cannot be determined.
fn default_config_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".docker").join("config.json"))
}

/// Testable inner implementation that accepts an explicit config path.
fn resolve_credentials_from(registry: &str, config_path: Option<PathBuf>) -> DockerCredentials {
    let Some(path) = config_path else {
        debug!("cannot determine home directory; returning empty credentials");
        return DockerCredentials::default();
    };

    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => {
            debug!("cannot read {}: {e}", path.display());
            return DockerCredentials::default();
        }
    };

    let config: DockerConfig = match serde_json::from_str(&content) {
        Ok(c) => c,
        Err(e) => {
            debug!("cannot parse {}: {e}", path.display());
            return DockerCredentials::default();
        }
    };

    resolve_from_config(registry, &config)
}

/// Walk the config resolution chain: auths -> credHelpers -> credsStore.
fn resolve_from_config(registry: &str, config: &DockerConfig) -> DockerCredentials {
    // 1. Inline auths
    if let Some(entry) = config.auths.get(registry)
        && let Some(creds) = decode_auth_field(entry.auth.as_deref())
    {
        debug!("resolved credentials for {registry} from inline auths");
        return creds;
    }

    // 2. Per-registry credential helper
    if let Some(helper) = config.cred_helpers.get(registry)
        && let Some(creds) = invoke_credential_helper(helper, registry)
    {
        debug!("resolved credentials for {registry} from credHelper '{helper}'");
        return creds;
    }

    // 3. Global credential store
    if let Some(helper) = &config.creds_store
        && let Some(creds) = invoke_credential_helper(helper, registry)
    {
        debug!("resolved credentials for {registry} from credsStore '{helper}'");
        return creds;
    }

    debug!("no credentials found for {registry}");
    DockerCredentials::default()
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Decode a base64-encoded `username:password` auth field.
fn decode_auth_field(auth: Option<&str>) -> Option<DockerCredentials> {
    let encoded = auth?.trim();
    if encoded.is_empty() {
        return None;
    }

    let decoded_bytes = BASE64.decode(encoded).ok()?;
    let decoded = String::from_utf8(decoded_bytes).ok()?;

    let (username, password) = decoded.split_once(':')?;

    Some(DockerCredentials {
        username: Some(username.to_owned()),
        password: Some(password.to_owned()),
    })
}

/// Shell out to `docker-credential-<helper> get` and parse the JSON response.
fn invoke_credential_helper(helper: &str, registry: &str) -> Option<DockerCredentials> {
    let binary = format!("docker-credential-{helper}");

    let mut child = Command::new(&binary)
        .arg("get")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(registry.as_bytes());
    }

    let output = child.wait_with_output().ok()?;
    if !output.status.success() {
        debug!("{binary} exited with status {}", output.status);
        return None;
    }

    let response: CredentialHelperResponse = serde_json::from_slice(&output.stdout).ok()?;

    // Credential helpers return "<token>" as the username for token-based
    // auth.  We treat any non-empty response as valid.
    let username = response.username.filter(|u| !u.is_empty());
    let password = response.secret.filter(|s| !s.is_empty());

    if username.is_none() && password.is_none() {
        return None;
    }

    Some(DockerCredentials { username, password })
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // decode_auth_field
    // -----------------------------------------------------------------------

    #[test]
    fn decode_valid_auth() {
        // "testuser:testpass" in base64
        let encoded = BASE64.encode("testuser:testpass");
        let creds = decode_auth_field(Some(&encoded)).unwrap();
        assert_eq!(creds.username.as_deref(), Some("testuser"));
        assert_eq!(creds.password.as_deref(), Some("testpass"));
    }

    #[test]
    fn decode_empty_auth_returns_none() {
        assert!(decode_auth_field(Some("")).is_none());
        assert!(decode_auth_field(None).is_none());
    }

    #[test]
    fn decode_invalid_base64_returns_none() {
        assert!(decode_auth_field(Some("not-valid-base64!!!")).is_none());
    }

    #[test]
    fn decode_missing_colon_returns_none() {
        // Base64 of "nocolon" (no `:` separator)
        let encoded = BASE64.encode("nocolon");
        assert!(decode_auth_field(Some(&encoded)).is_none());
    }

    #[test]
    fn decode_password_with_colons() {
        // Password containing colons: "user:p@ss:word:123"
        let encoded = BASE64.encode("user:p@ss:word:123");
        let creds = decode_auth_field(Some(&encoded)).unwrap();
        assert_eq!(creds.username.as_deref(), Some("user"));
        assert_eq!(creds.password.as_deref(), Some("p@ss:word:123"));
    }

    // -----------------------------------------------------------------------
    // resolve_from_config: inline auths
    // -----------------------------------------------------------------------

    #[test]
    fn resolve_inline_auths() {
        let json = format!(
            r#"{{
                "auths": {{
                    "ghcr.io": {{
                        "auth": "{}"
                    }}
                }}
            }}"#,
            BASE64.encode("myuser:mytoken")
        );
        let config: DockerConfig = serde_json::from_str(&json).unwrap();
        let creds = resolve_from_config("ghcr.io", &config);
        assert_eq!(creds.username.as_deref(), Some("myuser"));
        assert_eq!(creds.password.as_deref(), Some("mytoken"));
    }

    // -----------------------------------------------------------------------
    // resolve_credentials_from: missing config file
    // -----------------------------------------------------------------------

    #[test]
    fn missing_config_returns_empty() {
        let creds =
            resolve_credentials_from("ghcr.io", Some(PathBuf::from("/nonexistent/config.json")));
        assert!(creds.is_empty());
    }

    #[test]
    fn no_home_dir_returns_empty() {
        let creds = resolve_credentials_from("ghcr.io", None);
        assert!(creds.is_empty());
    }

    // -----------------------------------------------------------------------
    // resolve_from_config: unknown registry
    // -----------------------------------------------------------------------

    #[test]
    fn unknown_registry_returns_empty() {
        let json = format!(
            r#"{{
                "auths": {{
                    "docker.io": {{
                        "auth": "{}"
                    }}
                }}
            }}"#,
            BASE64.encode("user:pass")
        );
        let config: DockerConfig = serde_json::from_str(&json).unwrap();
        let creds = resolve_from_config("ghcr.io", &config);
        assert!(creds.is_empty());
    }

    // -----------------------------------------------------------------------
    // resolve_from_config: empty config
    // -----------------------------------------------------------------------

    #[test]
    fn empty_config_returns_empty() {
        let config: DockerConfig = serde_json::from_str("{}").unwrap();
        let creds = resolve_from_config("ghcr.io", &config);
        assert!(creds.is_empty());
    }

    // -----------------------------------------------------------------------
    // resolve_from_config: credHelpers and credsStore fallback
    // -----------------------------------------------------------------------

    #[test]
    fn cred_helpers_config_parsed() {
        let json = r#"{
            "credHelpers": {
                "gcr.io": "gcloud",
                "123456.dkr.ecr.us-east-1.amazonaws.com": "ecr-login"
            }
        }"#;
        let config: DockerConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.cred_helpers.get("gcr.io").unwrap(), "gcloud");
        assert_eq!(
            config
                .cred_helpers
                .get("123456.dkr.ecr.us-east-1.amazonaws.com")
                .unwrap(),
            "ecr-login"
        );
    }

    #[test]
    fn creds_store_config_parsed() {
        let json = r#"{ "credsStore": "desktop" }"#;
        let config: DockerConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.creds_store.as_deref(), Some("desktop"));
    }

    // -----------------------------------------------------------------------
    // resolve_from_config: auths takes precedence over helpers
    // -----------------------------------------------------------------------

    #[test]
    fn inline_auths_takes_precedence_over_helpers() {
        let json = format!(
            r#"{{
                "auths": {{
                    "ghcr.io": {{ "auth": "{}" }}
                }},
                "credHelpers": {{
                    "ghcr.io": "some-helper"
                }},
                "credsStore": "desktop"
            }}"#,
            BASE64.encode("inlineuser:inlinepass")
        );
        let config: DockerConfig = serde_json::from_str(&json).unwrap();
        let creds = resolve_from_config("ghcr.io", &config);
        assert_eq!(creds.username.as_deref(), Some("inlineuser"));
        assert_eq!(creds.password.as_deref(), Some("inlinepass"));
    }

    // -----------------------------------------------------------------------
    // resolve_credentials_from: real config file on disk
    // -----------------------------------------------------------------------

    #[test]
    fn resolve_from_tmpfile() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.json");
        let json = format!(
            r#"{{
                "auths": {{
                    "registry.example.com": {{
                        "auth": "{}"
                    }}
                }}
            }}"#,
            BASE64.encode("fileuser:filepass")
        );
        std::fs::write(&config_path, json).unwrap();

        let creds = resolve_credentials_from("registry.example.com", Some(config_path));
        assert_eq!(creds.username.as_deref(), Some("fileuser"));
        assert_eq!(creds.password.as_deref(), Some("filepass"));
    }

    // -----------------------------------------------------------------------
    // DockerCredentials::is_empty
    // -----------------------------------------------------------------------

    #[test]
    fn default_credentials_are_empty() {
        assert!(DockerCredentials::default().is_empty());
    }

    #[test]
    fn credentials_with_username_are_not_empty() {
        let creds = DockerCredentials {
            username: Some("u".to_owned()),
            password: None,
        };
        assert!(!creds.is_empty());
    }

    // -----------------------------------------------------------------------
    // Credential helper shell-out (requires `docker-credential-*` on PATH)
    // -----------------------------------------------------------------------

    #[test]
    #[ignore = "requires docker-credential-desktop on PATH"]
    fn credential_helper_invocation() {
        // This test requires `docker-credential-desktop` (or similar) on PATH.
        // Run manually: cargo test -p cella-features -- --ignored credential_helper
        let result = invoke_credential_helper("desktop", "https://index.docker.io/v1/");
        // We can't assert specific credentials, but we can verify the call
        // doesn't panic and returns a sensible shape.
        if let Some(creds) = result {
            assert!(!creds.is_empty(), "helper returned empty credentials");
        }
    }
}
