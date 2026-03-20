//! Container/image naming and label generation.

use std::collections::HashMap;
use std::path::Path;

use chrono::Utc;
use sha2::{Digest, Sha256};

/// Generate container name: `cella-<name|folder>-<hash8>`.
pub fn container_name(workspace_root: &Path, config_name: Option<&str>) -> String {
    let identifier = identifier_from(workspace_root, config_name);
    let hash = workspace_hash(workspace_root);
    format!("cella-{identifier}-{hash}")
}

/// Generate image name: `cella-img-<name|folder>-<hash8>`.
pub fn image_name(workspace_root: &Path, config_name: Option<&str>) -> String {
    let identifier = identifier_from(workspace_root, config_name);
    let hash = workspace_hash(workspace_root);
    format!("cella-img-{identifier}-{hash}")
}

fn identifier_from(workspace_root: &Path, config_name: Option<&str>) -> String {
    config_name.map_or_else(
        || {
            workspace_root.file_name().map_or_else(
                || "unnamed".to_string(),
                |n| sanitize_name(&n.to_string_lossy()),
            )
        },
        sanitize_name,
    )
}

/// Hash workspace path to short hex (first 8 chars of SHA256).
fn workspace_hash(workspace_root: &Path) -> String {
    let canonical = workspace_root
        .canonicalize()
        .unwrap_or_else(|_| workspace_root.to_path_buf());
    let hash = hex::encode(Sha256::digest(canonical.to_string_lossy().as_bytes()));
    hash[..8].to_string()
}

/// Sanitize a string for Docker container name compatibility.
fn sanitize_name(s: &str) -> String {
    let sanitized: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();

    // Collapse consecutive dashes
    let mut result = String::with_capacity(sanitized.len());
    let mut prev_dash = false;
    for c in sanitized.chars() {
        if c == '-' {
            if !prev_dash {
                result.push(c);
            }
            prev_dash = true;
        } else {
            result.push(c);
            prev_dash = false;
        }
    }

    result.trim_matches('-').to_string()
}

/// Generate image name for a features-layered build.
/// `features_digest` is the hex-encoded lowercase SHA256 (64-character string).
pub fn image_name_with_features(
    workspace_root: &Path,
    config_name: Option<&str>,
    features_digest: &str,
) -> String {
    let identifier = identifier_from(workspace_root, config_name);
    let path_hash = workspace_hash(workspace_root);
    let feat_hash = &features_digest[..8];
    format!("cella-img-{identifier}-{path_hash}-{feat_hash}")
}

/// Standard Docker labels for cella containers.
pub fn container_labels(
    workspace_root: &Path,
    config_path: &Path,
    config_hash: &str,
    docker_runtime: &str,
) -> HashMap<String, String> {
    let mut labels = HashMap::new();
    labels.insert("dev.cella.tool".to_string(), "cella".to_string());
    labels.insert(
        "dev.cella.workspace_path".to_string(),
        workspace_root
            .canonicalize()
            .unwrap_or_else(|_| workspace_root.to_path_buf())
            .to_string_lossy()
            .to_string(),
    );
    labels.insert(
        "dev.cella.config_path".to_string(),
        config_path
            .canonicalize()
            .unwrap_or_else(|_| config_path.to_path_buf())
            .to_string_lossy()
            .to_string(),
    );
    labels.insert("dev.cella.config_hash".to_string(), config_hash.to_string());
    labels.insert(
        "dev.cella.docker_runtime".to_string(),
        docker_runtime.to_string(),
    );
    labels.insert("dev.cella.created_at".to_string(), Utc::now().to_rfc3339());
    labels
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn container_name_with_config_name() {
        let path = PathBuf::from("/tmp/my-project");
        let name = container_name(&path, Some("my-app"));
        assert!(name.starts_with("cella-my-app-"));
        assert_eq!(name.len(), "cella-my-app-".len() + 8);
    }

    #[test]
    fn container_name_from_folder() {
        let path = PathBuf::from("/tmp/my-project");
        let name = container_name(&path, None);
        assert!(name.starts_with("cella-my-project-"));
    }

    #[test]
    fn image_name_format() {
        let path = PathBuf::from("/tmp/my-project");
        let name = image_name(&path, Some("test"));
        assert!(name.starts_with("cella-img-test-"));
    }

    #[test]
    fn hash_deterministic() {
        let path = PathBuf::from("/tmp/test-project");
        let h1 = workspace_hash(&path);
        let h2 = workspace_hash(&path);
        assert_eq!(h1, h2);
    }

    #[test]
    fn different_paths_different_hashes() {
        let h1 = workspace_hash(&PathBuf::from("/tmp/project-a"));
        let h2 = workspace_hash(&PathBuf::from("/tmp/project-b"));
        assert_ne!(h1, h2);
    }

    #[test]
    fn sanitize_special_chars() {
        assert_eq!(sanitize_name("my app@v2"), "my-app-v2");
    }

    #[test]
    fn sanitize_collapses_dashes() {
        assert_eq!(sanitize_name("a---b"), "a-b");
    }

    #[test]
    fn sanitize_trims_dashes() {
        assert_eq!(sanitize_name("-abc-"), "abc");
    }

    #[test]
    fn image_name_with_features_format() {
        let path = PathBuf::from("/tmp/my-project");
        let digest = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
        let name = image_name_with_features(&path, Some("test"), digest);
        assert!(name.starts_with("cella-img-test-"));
        assert!(name.len() > "cella-img-test-".len());
        assert!(name.ends_with(&digest[..8]));
    }

    #[test]
    fn image_name_with_features_hyphenated_identifier() {
        let path = PathBuf::from("/tmp/my-app");
        let digest = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
        let name = image_name_with_features(&path, Some("my-app"), digest);
        assert!(name.starts_with("cella-img-my-app-"));
        assert!(name.ends_with(&digest[..8]));
    }

    #[test]
    fn labels_contain_required_keys() {
        let labels = container_labels(
            &PathBuf::from("/tmp/test"),
            &PathBuf::from("/tmp/test/.devcontainer/devcontainer.json"),
            "abc123",
            "linux-native",
        );
        assert_eq!(labels["dev.cella.tool"], "cella");
        assert_eq!(labels["dev.cella.config_hash"], "abc123");
        assert_eq!(labels["dev.cella.docker_runtime"], "linux-native");
        assert!(labels.contains_key("dev.cella.workspace_path"));
        assert!(labels.contains_key("dev.cella.config_path"));
        assert!(labels.contains_key("dev.cella.created_at"));
    }

    #[test]
    fn labels_contain_docker_runtime() {
        let labels = container_labels(
            &PathBuf::from("/tmp/test"),
            &PathBuf::from("/tmp/test/.devcontainer/devcontainer.json"),
            "hash",
            "orbstack",
        );
        assert_eq!(labels["dev.cella.docker_runtime"], "orbstack");
    }
}
