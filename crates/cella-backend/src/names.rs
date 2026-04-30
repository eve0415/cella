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

/// Sanitize a string for container name compatibility.
///
/// Lowercases and replaces invalid characters, then collapses consecutive
/// separator chars (`[._-]`) into a single dash.
fn sanitize_name(s: &str) -> String {
    let sanitized: String = s
        .to_ascii_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();

    let mut result = String::with_capacity(sanitized.len());
    let mut prev_sep = false;
    for c in sanitized.chars() {
        let is_sep = c == '-' || c == '_' || c == '.';
        if is_sep {
            if !prev_sep {
                result.push('-');
            }
            prev_sep = true;
        } else {
            result.push(c);
            prev_sep = false;
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

/// Label key for the container backend kind.
pub const BACKEND_LABEL: &str = "dev.cella.backend";

/// Standard labels for cella containers.
///
/// Emits both cella-specific (`dev.cella.*`) and spec-standard
/// (`devcontainer.*`) labels so that VS Code and other tools can discover
/// cella-created containers.
pub fn container_labels(
    workspace_root: &Path,
    config_path: &Path,
    config_hash: &str,
    runtime_label: &str,
) -> HashMap<String, String> {
    let canonical_workspace = workspace_root
        .canonicalize()
        .unwrap_or_else(|_| workspace_root.to_path_buf());
    let canonical_config = config_path
        .canonicalize()
        .unwrap_or_else(|_| config_path.to_path_buf());
    let workspace_str = canonical_workspace.to_string_lossy().to_string();
    let config_str = canonical_config.to_string_lossy().to_string();

    let mut labels = HashMap::new();

    // Cella-specific labels.
    labels.insert("dev.cella.tool".to_string(), "cella".to_string());
    labels.insert(
        "dev.cella.workspace_path".to_string(),
        workspace_str.clone(),
    );
    labels.insert("dev.cella.config_path".to_string(), config_str.clone());
    labels.insert("dev.cella.config_hash".to_string(), config_hash.to_string());
    labels.insert(
        "dev.cella.docker_runtime".to_string(),
        runtime_label.to_string(),
    );
    labels.insert("dev.cella.created_at".to_string(), Utc::now().to_rfc3339());

    // Spec-standard labels for VS Code / tooling interop.
    labels.insert("devcontainer.local_folder".to_string(), workspace_str);
    labels.insert("devcontainer.config_file".to_string(), config_str);

    labels
}

/// Generate compose project name: `cella-<name|folder>-<hash8>`.
pub fn compose_project_name(workspace_root: &Path, config_name: Option<&str>) -> String {
    let identifier = identifier_from(workspace_root, config_name);
    let hash = workspace_hash(workspace_root);
    format!("cella-{identifier}-{hash}")
}

/// Labels for compose-managed devcontainers.
pub fn compose_labels(
    workspace_root: &Path,
    config_path: &Path,
    config_hash: &str,
    runtime_label: &str,
    project_name: &str,
    primary_service: &str,
) -> HashMap<String, String> {
    let mut labels = container_labels(workspace_root, config_path, config_hash, runtime_label);
    labels.insert(
        "dev.cella.compose_project".to_string(),
        project_name.to_string(),
    );
    labels.insert(
        "dev.cella.primary_service".to_string(),
        primary_service.to_string(),
    );
    labels
}

/// Additional labels for worktree-backed containers.
pub fn worktree_labels(branch_name: &str, parent_repo: &Path) -> HashMap<String, String> {
    let mut labels = HashMap::new();
    labels.insert("dev.cella.worktree".to_string(), "true".to_string());
    labels.insert("dev.cella.branch".to_string(), branch_name.to_string());
    labels.insert(
        "dev.cella.parent_repo".to_string(),
        parent_repo
            .canonicalize()
            .unwrap_or_else(|_| parent_repo.to_path_buf())
            .to_string_lossy()
            .to_string(),
    );
    labels
}

/// Compute a SHA-256 digest of the features config for image tagging.
pub fn compute_features_digest(config: &serde_json::Value) -> String {
    let features = config.get("features").unwrap_or(&serde_json::Value::Null);
    let canonical = serde_json::to_string(features).unwrap_or_default();
    hex::encode(Sha256::digest(canonical.as_bytes()))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

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
    fn sanitize_lowercases() {
        assert_eq!(sanitize_name("ActivityManager"), "activitymanager");
    }

    #[test]
    fn sanitize_mixed_separators() {
        assert_eq!(sanitize_name("foo._bar"), "foo-bar");
    }

    #[test]
    fn image_name_always_lowercase() {
        let path = PathBuf::from("/tmp/MyProject");
        let name = image_name(&path, Some("MyApp"));
        assert_eq!(name, name.to_lowercase());
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
    fn labels_contain_spec_standard_keys() {
        let labels = container_labels(
            &PathBuf::from("/tmp/test"),
            &PathBuf::from("/tmp/test/.devcontainer/devcontainer.json"),
            "abc123",
            "linux-native",
        );
        assert!(labels.contains_key("devcontainer.local_folder"));
        assert!(labels.contains_key("devcontainer.config_file"));
        // Spec labels must match cella equivalents.
        assert_eq!(
            labels["devcontainer.local_folder"],
            labels["dev.cella.workspace_path"]
        );
        assert_eq!(
            labels["devcontainer.config_file"],
            labels["dev.cella.config_path"]
        );
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

    #[test]
    fn worktree_labels_contain_required_keys() {
        let labels = worktree_labels("feature/auth", &PathBuf::from("/tmp/repo"));
        assert_eq!(labels["dev.cella.worktree"], "true");
        assert_eq!(labels["dev.cella.branch"], "feature/auth");
        assert!(labels.contains_key("dev.cella.parent_repo"));
    }

    #[test]
    fn worktree_labels_preserve_branch_name() {
        let labels = worktree_labels("feature/auth/oauth2", &PathBuf::from("/tmp/repo"));
        assert_eq!(labels["dev.cella.branch"], "feature/auth/oauth2");
    }

    #[test]
    fn compose_project_name_format() {
        let path = PathBuf::from("/tmp/my-project");
        let name = compose_project_name(&path, Some("web"));
        assert!(name.starts_with("cella-web-"));
        assert_eq!(name.len(), "cella-web-".len() + 8);
    }

    #[test]
    fn compose_project_name_from_folder() {
        let path = PathBuf::from("/tmp/my-project");
        let name = compose_project_name(&path, None);
        assert!(name.starts_with("cella-my-project-"));
    }

    #[test]
    fn compose_labels_contain_base_and_compose_keys() {
        let labels = compose_labels(
            &PathBuf::from("/tmp/test"),
            &PathBuf::from("/tmp/test/.devcontainer/devcontainer.json"),
            "abc123",
            "linux-native",
            "cella-test-12345678",
            "app",
        );
        // Base container labels must be present.
        assert_eq!(labels["dev.cella.tool"], "cella");
        assert_eq!(labels["dev.cella.config_hash"], "abc123");
        assert_eq!(labels["dev.cella.docker_runtime"], "linux-native");
        // Compose-specific labels.
        assert_eq!(labels["dev.cella.compose_project"], "cella-test-12345678");
        assert_eq!(labels["dev.cella.primary_service"], "app");
    }

    #[test]
    fn identifier_from_root_path() {
        // When workspace_root has no file_name (e.g., "/"), use "unnamed".
        let path = PathBuf::from("/");
        let name = container_name(&path, None);
        assert!(name.starts_with("cella-unnamed-"));
    }
}
