use std::path::Path;

use serde_json::Value;
use tracing::debug;

use super::error::CellaConfigError;

pub fn load_layer(dir: &Path, stem: &str) -> Result<Option<Value>, CellaConfigError> {
    let toml_path = dir.join(format!("{stem}.toml"));
    if toml_path.is_file() {
        debug!("Loading cella config from {}", toml_path.display());
        return load_toml(&toml_path).map(Some);
    }

    let json_path = dir.join(format!("{stem}.json"));
    if json_path.is_file() {
        debug!("Loading cella config from {}", json_path.display());
        return load_json(&json_path).map(Some);
    }

    Ok(None)
}

fn load_toml(path: &Path) -> Result<Value, CellaConfigError> {
    let content = std::fs::read_to_string(path).map_err(|e| CellaConfigError::ReadFile {
        path: path.to_path_buf(),
        source: e,
    })?;
    let toml_val: toml::Value =
        toml::from_str(&content).map_err(|e| CellaConfigError::ParseToml {
            path: path.to_path_buf(),
            source: e,
        })?;
    serde_json::to_value(toml_val).map_err(|e| CellaConfigError::Deserialization { source: e })
}

fn load_json(path: &Path) -> Result<Value, CellaConfigError> {
    let content = std::fs::read_to_string(path).map_err(|e| CellaConfigError::ReadFile {
        path: path.to_path_buf(),
        source: e,
    })?;
    let stripped = cella_jsonc::strip(&content).map_err(|e| CellaConfigError::JsoncStrip {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;
    serde_json::from_str(&stripped).map_err(|e| CellaConfigError::ParseJson {
        path: path.to_path_buf(),
        source: e,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn toml_only() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("config.toml"),
            "[security]\nmode = \"enforced\"\n",
        )
        .unwrap();
        let val = load_layer(tmp.path(), "config").unwrap().unwrap();
        assert_eq!(val["security"]["mode"], "enforced");
    }

    #[test]
    fn json_only() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("config.json"),
            r#"{"security": {"mode": "logged"}}"#,
        )
        .unwrap();
        let val = load_layer(tmp.path(), "config").unwrap().unwrap();
        assert_eq!(val["security"]["mode"], "logged");
    }

    #[test]
    fn toml_preferred_over_json() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("config.toml"),
            "[security]\nmode = \"enforced\"\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("config.json"),
            r#"{"security": {"mode": "logged"}}"#,
        )
        .unwrap();
        let val = load_layer(tmp.path(), "config").unwrap().unwrap();
        assert_eq!(val["security"]["mode"], "enforced");
    }

    #[test]
    fn neither_returns_none() {
        let tmp = TempDir::new().unwrap();
        let val = load_layer(tmp.path(), "config").unwrap();
        assert!(val.is_none());
    }

    #[test]
    fn invalid_toml_hard_fails() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("config.toml"), "not valid {{{").unwrap();
        let err = load_layer(tmp.path(), "config").unwrap_err();
        assert!(matches!(err, CellaConfigError::ParseToml { .. }));
    }

    #[test]
    fn invalid_json_hard_fails() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("config.json"), "{not valid}").unwrap();
        let err = load_layer(tmp.path(), "config").unwrap_err();
        assert!(matches!(err, CellaConfigError::ParseJson { .. }));
    }

    #[test]
    fn jsonc_comments_supported() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("cella.json"),
            r#"{
                // This is a comment
                "security": {
                    "mode": "enforced" /* inline comment */
                }
            }"#,
        )
        .unwrap();
        let val = load_layer(tmp.path(), "cella").unwrap().unwrap();
        assert_eq!(val["security"]["mode"], "enforced");
    }

    #[test]
    fn unterminated_block_comment_fails() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("config.json"), r#"{"a": 1 /* unterminated"#).unwrap();
        let err = load_layer(tmp.path(), "config").unwrap_err();
        assert!(matches!(err, CellaConfigError::JsoncStrip { .. }));
    }
}
