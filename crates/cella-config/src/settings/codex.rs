use serde::Deserialize;

const fn default_true() -> bool {
    true
}

fn default_latest() -> String {
    "latest".to_string()
}

/// Where Codex keeps its `SQLite` databases.
///
/// Codex opens every runtime database in WAL mode, and WAL requires all accessors to share one kernel's page cache for the mmapped `-shm` wal-index.
/// A forwarded `~/.codex` crosses the host/VM boundary, so the host Codex app and a container reset each other's WAL and corrupt the databases.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CodexDatabase {
    /// Keep the databases on container-local storage, outside the forwarded directory.
    ///
    /// Session transcripts still live in the forwarded `~/.codex/sessions`, and Codex rebuilds its thread index from them, so history survives a container rebuild.
    #[default]
    Container,

    /// Leave the databases in the forwarded host directory.
    ///
    /// This is the configuration that corrupts them, and it exists only as an escape hatch.
    Host,
}

/// `OpenAI` Codex CLI tool settings.
///
/// Controls config forwarding and version for the Codex CLI inside dev containers.
/// Installation is triggered via `cella install` or `[tools] install = ["codex"]`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Codex {
    /// Bind-mount `~/.codex` from host into the container (default: true).
    #[serde(default = "default_true")]
    pub forward_config: bool,

    /// Where Codex keeps its `SQLite` databases (default: `container`).
    #[serde(default)]
    pub database: CodexDatabase,

    /// Version to install: `"latest"` or pinned e.g. `"0.1.2"`.
    #[serde(default = "default_latest")]
    pub version: String,
}

impl Default for Codex {
    fn default() -> Self {
        Self {
            forward_config: true,
            database: CodexDatabase::Container,
            version: "latest".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_values() {
        let settings = Codex::default();
        assert!(settings.forward_config);
        assert_eq!(settings.database, CodexDatabase::Container);
        assert_eq!(settings.version, "latest");
    }

    #[test]
    fn deserialize_empty_uses_defaults() {
        let settings: Codex = toml::from_str("").unwrap();
        assert!(settings.forward_config);
        assert_eq!(settings.database, CodexDatabase::Container);
        assert_eq!(settings.version, "latest");
    }

    #[test]
    fn deserialize_forward_config_disabled() {
        let settings: Codex = toml::from_str("forward_config = false").unwrap();
        assert!(!settings.forward_config);
    }

    #[test]
    fn deserialize_database_host() {
        let settings: Codex = toml::from_str(r#"database = "host""#).unwrap();
        assert_eq!(settings.database, CodexDatabase::Host);
    }

    #[test]
    fn deserialize_database_container() {
        let settings: Codex = toml::from_str(r#"database = "container""#).unwrap();
        assert_eq!(settings.database, CodexDatabase::Container);
    }

    #[test]
    fn rejects_unknown_database_value() {
        assert!(toml::from_str::<Codex>(r#"database = "shared""#).is_err());
    }

    #[test]
    fn deserialize_pinned_version() {
        let settings: Codex = toml::from_str("version = \"0.1.2\"").unwrap();
        assert_eq!(settings.version, "0.1.2");
    }

    #[test]
    fn rejects_unknown_fields() {
        assert!(toml::from_str::<Codex>("enabled = true").is_err());
    }
}
