//! Parse `devcontainer.metadata` labels from base images.

use crate::types::{FeatureContainerConfig, LifecycleEntry};

use super::helpers::{deep_merge, extend_dedup};

/// User-related properties extracted from image metadata.
#[derive(Debug, Clone, Default)]
pub struct ImageMetadataUserInfo {
    pub remote_user: Option<String>,
    pub container_user: Option<String>,
}

/// Apply collection fields (containerEnv, mounts, capAdd, securityOpt, etc.) from a
/// single metadata entry onto the accumulated config.
fn apply_collections(config: &mut FeatureContainerConfig, entry: &serde_json::Value) {
    // containerEnv: merge maps, later overrides earlier for same key
    if let Some(env) = entry.get("containerEnv").and_then(|v| v.as_object()) {
        for (k, v) in env {
            if let Some(s) = v.as_str() {
                config.container_env.insert(k.clone(), s.to_string());
            }
        }
    }

    // mounts: accumulate
    if let Some(mounts) = entry.get("mounts").and_then(|v| v.as_array()) {
        for m in mounts {
            if let Some(s) = m.as_str() {
                config.mounts.push(s.to_string());
            }
        }
    }

    // capAdd, securityOpt: accumulate + deduplicate
    if let Some(caps) = entry.get("capAdd").and_then(|v| v.as_array()) {
        let strs: Vec<String> = caps
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();
        extend_dedup(&mut config.cap_add, &strs);
    }
    if let Some(sec) = entry.get("securityOpt").and_then(|v| v.as_array()) {
        let strs: Vec<String> = sec
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();
        extend_dedup(&mut config.security_opt, &strs);
    }

    // privileged, init: OR
    if entry.get("privileged").and_then(serde_json::Value::as_bool) == Some(true) {
        config.privileged = true;
    }
    if entry.get("init").and_then(serde_json::Value::as_bool) == Some(true) {
        config.init = true;
    }

    // entrypoint
    if let Some(ep) = entry.get("entrypoint").and_then(|v| v.as_str()) {
        config.entrypoints.push(ep.to_string());
    }

    // customizations: deep merge
    if let Some(cust) = entry.get("customizations") {
        config.customizations = deep_merge(&config.customizations, cust);
    }
}

/// Accessor function that returns a mutable reference to a lifecycle phase's entry list.
type LifecycleAccessor = fn(&mut crate::types::FeatureLifecycle) -> &mut Vec<LifecycleEntry>;

/// Apply lifecycle commands from a single metadata entry onto the accumulated config.
fn apply_lifecycle(config: &mut FeatureContainerConfig, entry: &serde_json::Value) {
    let origin = entry
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("image-metadata")
        .to_string();

    let phases: &[(&str, LifecycleAccessor)] = &[
        ("onCreateCommand", |lc| &mut lc.on_create),
        ("postCreateCommand", |lc| &mut lc.post_create),
        ("postStartCommand", |lc| &mut lc.post_start),
        ("postAttachCommand", |lc| &mut lc.post_attach),
    ];
    for &(key, accessor) in phases {
        if let Some(cmd) = entry.get(key)
            && !cmd.is_null()
        {
            accessor(&mut config.lifecycle).push(LifecycleEntry {
                origin: origin.clone(),
                command: cmd.clone(),
            });
        }
    }
}

/// Parse image metadata JSON into container config and user info.
///
/// The metadata is a JSON array of objects. Each entry may contain
/// `remoteUser`, `containerUser`, `containerEnv`, `mounts`, lifecycle
/// commands, etc. Later entries override earlier ones for scalar values;
/// collections accumulate.
pub fn parse_image_metadata(
    metadata_json: &str,
) -> (FeatureContainerConfig, ImageMetadataUserInfo) {
    let entries: Vec<serde_json::Value> = serde_json::from_str(metadata_json).unwrap_or_default();

    let mut config = FeatureContainerConfig::default();
    let mut user_info = ImageMetadataUserInfo::default();

    for entry in &entries {
        // remoteUser / containerUser: last one wins
        if let Some(u) = entry.get("remoteUser").and_then(|v| v.as_str()) {
            user_info.remote_user = Some(u.to_string());
        }
        if let Some(u) = entry.get("containerUser").and_then(|v| v.as_str()) {
            user_info.container_user = Some(u.to_string());
        }

        apply_collections(&mut config, entry);
        apply_lifecycle(&mut config, entry);
    }

    (config, user_info)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn extracts_remote_user() {
        let meta = json!([{"remoteUser": "vscode"}]).to_string();
        let (_, user_info) = parse_image_metadata(&meta);
        assert_eq!(user_info.remote_user.as_deref(), Some("vscode"));
        assert_eq!(user_info.container_user, None);
    }

    #[test]
    fn last_remote_user_wins() {
        let meta = json!([
            {"remoteUser": "root"},
            {"id": "feature-1"},
            {"remoteUser": "vscode"}
        ])
        .to_string();
        let (_, user_info) = parse_image_metadata(&meta);
        assert_eq!(user_info.remote_user.as_deref(), Some("vscode"));
    }

    #[test]
    fn extracts_container_user() {
        let meta = json!([{"containerUser": "node"}]).to_string();
        let (_, user_info) = parse_image_metadata(&meta);
        assert_eq!(user_info.container_user.as_deref(), Some("node"));
    }

    #[test]
    fn merges_container_env() {
        let meta = json!([
            {"containerEnv": {"A": "1", "B": "2"}},
            {"containerEnv": {"B": "3", "C": "4"}}
        ])
        .to_string();
        let (config, _) = parse_image_metadata(&meta);
        assert_eq!(config.container_env.get("A").unwrap(), "1");
        assert_eq!(config.container_env.get("B").unwrap(), "3"); // later wins
        assert_eq!(config.container_env.get("C").unwrap(), "4");
    }

    #[test]
    fn accumulates_mounts() {
        let meta = json!([{"mounts": ["source=a,target=/a"]}, {"mounts": ["source=b,target=/b"]}])
            .to_string();
        let (config, _) = parse_image_metadata(&meta);
        assert_eq!(config.mounts.len(), 2);
    }

    #[test]
    fn deduplicates_cap_add() {
        let meta = json!([{"capAdd": ["SYS_PTRACE"]}, {"capAdd": ["SYS_PTRACE", "NET_ADMIN"]}])
            .to_string();
        let (config, _) = parse_image_metadata(&meta);
        assert_eq!(config.cap_add, vec!["SYS_PTRACE", "NET_ADMIN"]);
    }

    #[test]
    fn lifecycle_commands_with_origin() {
        let meta = json!([
            {"id": "common-utils", "postCreateCommand": "echo hello"},
            {"postStartCommand": "echo start"}
        ])
        .to_string();
        let (config, _) = parse_image_metadata(&meta);
        assert_eq!(config.lifecycle.post_create.len(), 1);
        assert_eq!(config.lifecycle.post_create[0].origin, "common-utils");
        assert_eq!(config.lifecycle.post_start.len(), 1);
        assert_eq!(config.lifecycle.post_start[0].origin, "image-metadata");
    }

    #[test]
    fn deep_merges_customizations() {
        let meta = json!([
            {"customizations": {"vscode": {"extensions": ["ext1"]}}},
            {"customizations": {"vscode": {"settings": {"a": 1}}}}
        ])
        .to_string();
        let (config, _) = parse_image_metadata(&meta);
        assert!(config.customizations["vscode"]["extensions"].is_array());
        assert!(config.customizations["vscode"]["settings"].is_object());
    }

    #[test]
    fn empty_metadata_returns_defaults() {
        let (config, user_info) = parse_image_metadata("");
        assert!(config.mounts.is_empty());
        assert!(config.container_env.is_empty());
        assert_eq!(user_info.remote_user, None);
        assert_eq!(user_info.container_user, None);
    }

    #[test]
    fn malformed_json_returns_defaults() {
        let (config, user_info) = parse_image_metadata("{not valid json");
        assert!(config.mounts.is_empty());
        assert_eq!(user_info.remote_user, None);
    }

    #[test]
    fn privileged_and_init_or() {
        let meta = json!([{"privileged": false, "init": true}, {"privileged": true}]).to_string();
        let (config, _) = parse_image_metadata(&meta);
        assert!(config.privileged);
        assert!(config.init);
    }
}
