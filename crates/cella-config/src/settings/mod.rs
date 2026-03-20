mod credentials;

use std::path::Path;

use serde::Deserialize;
use tracing::debug;

pub use credentials::CredentialSettings;

/// Cella's own settings, loaded from TOML config files.
///
/// Global config: `~/.cella/config.toml`
/// Project config: `<workspace>/.devcontainer/cella.toml`
///
/// Project settings override global settings per-key.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct CellaSettings {
    /// Credential forwarding settings.
    #[serde(default)]
    pub credentials: CredentialSettings,
}

impl CellaSettings {
    /// Load settings by merging global (`~/.cella/config.toml`) and
    /// project (`.devcontainer/cella.toml`) config files.
    ///
    /// Missing files are silently ignored. Parse errors log a warning
    /// and fall back to defaults.
    pub fn load(workspace_root: &Path) -> Self {
        let global = Self::load_global();
        let project = Self::load_project(workspace_root);

        Self::merge(global, project)
    }

    fn load_global() -> Option<Self> {
        let home = std::env::var("HOME").ok()?;
        let path = std::path::Path::new(&home)
            .join(".cella")
            .join("config.toml");
        Self::load_file(&path)
    }

    fn load_project(workspace_root: &Path) -> Option<Self> {
        let path = workspace_root.join(".devcontainer").join("cella.toml");
        Self::load_file(&path)
    }

    fn load_file(path: &Path) -> Option<Self> {
        let Ok(content) = std::fs::read_to_string(path) else {
            return None;
        };
        debug!("Loading cella settings from {}", path.display());
        match toml::from_str(&content) {
            Ok(settings) => Some(settings),
            Err(e) => {
                tracing::warn!("Failed to parse {}: {e}", path.display());
                None
            }
        }
    }

    /// Merge global and project settings. Project overrides global per-key.
    fn merge(global: Option<Self>, project: Option<Self>) -> Self {
        match (global, project) {
            (None, None) => Self::default(),
            (Some(g), None) => g,
            (None, Some(p)) => p,
            (Some(_global), Some(p)) => Self {
                credentials: CredentialSettings {
                    gh: p.credentials.gh,
                    // For future fields: if a project config explicitly sets a value,
                    // it wins. The current TOML deserialization with defaults means
                    // the project file's parsed value always takes precedence when
                    // the project file exists.
                },
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn default_settings() {
        let settings = CellaSettings::default();
        assert!(settings.credentials.gh);
    }

    #[test]
    fn load_missing_files_returns_defaults() {
        let tmp = TempDir::new().unwrap();
        let settings = CellaSettings::load(tmp.path());
        assert!(settings.credentials.gh);
    }

    #[test]
    fn load_project_config() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".devcontainer");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("cella.toml"), "[credentials]\ngh = false\n").unwrap();

        let settings = CellaSettings::load(tmp.path());
        assert!(!settings.credentials.gh);
    }

    #[test]
    fn load_empty_toml_uses_defaults() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".devcontainer");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("cella.toml"), "").unwrap();

        let settings = CellaSettings::load(tmp.path());
        assert!(settings.credentials.gh);
    }

    #[test]
    fn parse_full_config() {
        let toml_str = r"
[credentials]
gh = false
";
        let settings: CellaSettings = toml::from_str(toml_str).unwrap();
        assert!(!settings.credentials.gh);
    }

    #[test]
    fn merge_project_overrides_global() {
        let global = CellaSettings {
            credentials: CredentialSettings { gh: true },
        };
        let project = CellaSettings {
            credentials: CredentialSettings { gh: false },
        };
        let merged = CellaSettings::merge(Some(global), Some(project));
        assert!(!merged.credentials.gh);
    }

    #[test]
    fn merge_global_only() {
        let global = CellaSettings {
            credentials: CredentialSettings { gh: false },
        };
        let merged = CellaSettings::merge(Some(global), None);
        assert!(!merged.credentials.gh);
    }

    #[test]
    fn merge_neither() {
        let merged = CellaSettings::merge(None, None);
        assert!(merged.credentials.gh);
    }

    #[test]
    fn invalid_toml_falls_back_to_none() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".devcontainer");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("cella.toml"), "not valid toml {{{").unwrap();

        // Should not panic, just return defaults
        let settings = CellaSettings::load(tmp.path());
        assert!(settings.credentials.gh);
    }
}
