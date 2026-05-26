//! Persistent storage for user-approved custom credential providers.
//!
//! Custom providers defined in `cella.toml` can route credentials to
//! arbitrary domains. Before activating a custom provider, the user
//! must explicitly approve its full configuration. Any field change
//! invalidates the approval and requires re-consent.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use chrono::Utc;
use serde::{Deserialize, Serialize};

use cella_config::settings::CustomCredentialProvider;

/// A single approved custom credential provider with all fields frozen
/// at the time of approval.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovedProviderEntry {
    pub name: String,
    pub env: String,
    pub domains: Vec<String>,
    pub header: String,
    pub prefix: String,
    /// ISO 8601 timestamp of when the user approved this provider.
    pub approved_at: String,
}

/// On-disk collection of approved custom credential providers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovedProviders {
    pub schema_version: u32,
    pub approved: Vec<ApprovedProviderEntry>,
}

impl ApprovedProviders {
    /// Load approved providers from disk. Returns an empty list on
    /// missing or corrupt files.
    pub fn load(path: &Path) -> Self {
        let empty = Self {
            schema_version: 1,
            approved: Vec::new(),
        };

        let Ok(content) = fs::read_to_string(path) else {
            return empty;
        };

        serde_json::from_str(&content).unwrap_or(empty)
    }

    /// Persist approved providers to disk with restrictive permissions.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be written or the parent
    /// directory cannot be created.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let json = serde_json::to_string_pretty(self).map_err(io::Error::other)?;

        fs::write(path, json)?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
        }

        Ok(())
    }

    /// Check whether a provider's current configuration matches an
    /// existing approval. All fields must match exactly.
    #[must_use]
    pub fn is_approved(&self, provider: &CustomCredentialProvider) -> bool {
        self.approved.iter().any(|entry| {
            entry.name == provider.name
                && entry.env == provider.env
                && entry.domains == provider.domains
                && entry.header == provider.header
                && entry.prefix == provider.prefix
        })
    }

    /// Record approval for a custom provider, snapshotting all fields.
    pub fn approve(&mut self, provider: &CustomCredentialProvider) {
        // Remove any stale entry for the same name before adding.
        self.approved.retain(|e| e.name != provider.name);

        self.approved.push(ApprovedProviderEntry {
            name: provider.name.clone(),
            env: provider.env.clone(),
            domains: provider.domains.clone(),
            header: provider.header.clone(),
            prefix: provider.prefix.clone(),
            approved_at: Utc::now().to_rfc3339(),
        });
    }
}

/// Default path for the approved-providers file (`~/.cella/approved-providers.json`).
#[must_use]
pub fn default_path() -> Option<PathBuf> {
    cella_env::paths::cella_data_dir().map(|d| d.join("approved-providers.json"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn sample_provider(name: &str) -> CustomCredentialProvider {
        CustomCredentialProvider {
            name: name.to_string(),
            env: "API_KEY".to_string(),
            domains: vec!["api.example.com".to_string()],
            header: "Authorization".to_string(),
            prefix: "Bearer ".to_string(),
        }
    }

    #[test]
    fn load_missing_file_returns_empty() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nonexistent.json");

        let providers = ApprovedProviders::load(&path);
        assert_eq!(providers.schema_version, 1);
        assert!(providers.approved.is_empty());
    }

    #[test]
    fn load_corrupt_file_returns_empty() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("bad.json");
        fs::write(&path, "not valid json {{{").unwrap();

        let providers = ApprovedProviders::load(&path);
        assert_eq!(providers.schema_version, 1);
        assert!(providers.approved.is_empty());
    }

    #[test]
    fn save_and_reload_roundtrip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("providers.json");

        let mut providers = ApprovedProviders::load(&path);
        providers.approve(&sample_provider("test-api"));
        providers.save(&path).unwrap();

        let reloaded = ApprovedProviders::load(&path);
        assert_eq!(reloaded.schema_version, 1);
        assert_eq!(reloaded.approved.len(), 1);
        assert_eq!(reloaded.approved[0].name, "test-api");
        assert_eq!(reloaded.approved[0].env, "API_KEY");
        assert_eq!(
            reloaded.approved[0].domains,
            vec!["api.example.com".to_string()]
        );
        assert_eq!(reloaded.approved[0].header, "Authorization");
        assert_eq!(reloaded.approved[0].prefix, "Bearer ");
    }

    #[test]
    fn is_approved_matches_all_fields() {
        let mut providers = ApprovedProviders::load(Path::new("/nonexistent"));
        let provider = sample_provider("match-test");
        providers.approve(&provider);

        assert!(providers.is_approved(&provider));
    }

    #[test]
    fn is_approved_rejects_name_change() {
        let mut providers = ApprovedProviders::load(Path::new("/nonexistent"));
        providers.approve(&sample_provider("original"));

        let mut changed = sample_provider("original");
        changed.name = "renamed".to_string();
        assert!(!providers.is_approved(&changed));
    }

    #[test]
    fn is_approved_rejects_env_change() {
        let mut providers = ApprovedProviders::load(Path::new("/nonexistent"));
        let provider = sample_provider("env-test");
        providers.approve(&provider);

        let mut changed = provider.clone();
        changed.env = "DIFFERENT_KEY".to_string();
        assert!(!providers.is_approved(&changed));
    }

    #[test]
    fn is_approved_rejects_domain_change() {
        let mut providers = ApprovedProviders::load(Path::new("/nonexistent"));
        let provider = sample_provider("domain-test");
        providers.approve(&provider);

        let mut changed = provider.clone();
        changed.domains = vec!["evil.example.com".to_string()];
        assert!(!providers.is_approved(&changed));
    }

    #[test]
    fn is_approved_rejects_header_change() {
        let mut providers = ApprovedProviders::load(Path::new("/nonexistent"));
        let provider = sample_provider("header-test");
        providers.approve(&provider);

        let mut changed = provider.clone();
        changed.header = "X-Custom-Auth".to_string();
        assert!(!providers.is_approved(&changed));
    }

    #[test]
    fn is_approved_rejects_prefix_change() {
        let mut providers = ApprovedProviders::load(Path::new("/nonexistent"));
        let provider = sample_provider("prefix-test");
        providers.approve(&provider);

        let mut changed = provider.clone();
        changed.prefix = "Token ".to_string();
        assert!(!providers.is_approved(&changed));
    }

    #[test]
    fn approve_adds_entry_with_timestamp() {
        let mut providers = ApprovedProviders::load(Path::new("/nonexistent"));
        let provider = sample_provider("ts-test");
        providers.approve(&provider);

        assert_eq!(providers.approved.len(), 1);
        // Timestamp should be a valid RFC 3339 string (basic sanity check).
        assert!(providers.approved[0].approved_at.contains('T'));
    }

    #[test]
    fn approve_replaces_stale_entry_for_same_name() {
        let mut providers = ApprovedProviders::load(Path::new("/nonexistent"));
        let original = sample_provider("evolving");
        providers.approve(&original);

        let mut updated = original;
        updated.env = "NEW_KEY".to_string();
        providers.approve(&updated);

        assert_eq!(providers.approved.len(), 1);
        assert_eq!(providers.approved[0].env, "NEW_KEY");
    }

    #[test]
    fn schema_version_is_one() {
        let providers = ApprovedProviders::load(Path::new("/nonexistent"));
        assert_eq!(providers.schema_version, 1);
    }

    #[cfg(unix)]
    #[test]
    fn save_sets_restrictive_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new().unwrap();
        let path = dir.path().join("perms.json");

        let providers = ApprovedProviders::load(Path::new("/nonexistent"));
        providers.save(&path).unwrap();

        let metadata = fs::metadata(&path).unwrap();
        let mode = metadata.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn default_path_points_to_cella_dir() {
        if std::env::var("HOME").is_ok() {
            let path = default_path().unwrap();
            assert!(path.ends_with("approved-providers.json"));
            assert!(path.to_string_lossy().contains(".cella"));
        }
    }
}
