use std::path::Path;

use super::MountConfig;

pub(super) fn map_workspace_mount(
    config: &serde_json::Value,
    workspace_root: &Path,
    workspace_folder: &str,
) -> Option<MountConfig> {
    if let Some(mount_str) = config.get("workspaceMount").and_then(|v| v.as_str()) {
        if mount_str.is_empty() {
            return None; // Explicitly disabled
        }
        return parse_mount_string(mount_str);
    }

    // Default workspace mount
    Some(MountConfig {
        mount_type: "bind".to_string(),
        source: workspace_root
            .canonicalize()
            .unwrap_or_else(|_| workspace_root.to_path_buf())
            .to_string_lossy()
            .to_string(),
        target: workspace_folder.to_string(),
        consistency: Some("cached".to_string()),
    })
}

pub(super) fn map_additional_mounts(config: &serde_json::Value) -> Vec<MountConfig> {
    let Some(mounts) = config.get("mounts").and_then(|v| v.as_array()) else {
        return Vec::new();
    };

    mounts
        .iter()
        .filter_map(|m| match m {
            serde_json::Value::String(s) => parse_mount_string(s),
            serde_json::Value::Object(obj) => {
                let mount_type = obj
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("bind")
                    .to_string();
                let source = obj
                    .get("source")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let target = obj
                    .get("target")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();

                if target.is_empty() {
                    return None;
                }

                Some(MountConfig {
                    mount_type,
                    source,
                    target,
                    consistency: None,
                })
            }
            _ => None,
        })
        .collect()
}

pub(super) fn parse_mount_string(s: &str) -> Option<MountConfig> {
    let mut mount_type = "bind".to_string();
    let mut source = String::new();
    let mut target = String::new();
    let mut consistency = None;

    for part in s.split(',') {
        if let Some((key, value)) = part.split_once('=') {
            match key.trim() {
                "type" => mount_type = value.to_string(),
                "source" | "src" => source = value.to_string(),
                "target" | "dst" | "destination" => target = value.to_string(),
                "consistency" => consistency = Some(value.to_string()),
                _ => {}
            }
        }
    }

    if target.is_empty() {
        return None;
    }

    Some(MountConfig {
        mount_type,
        source,
        target,
        consistency,
    })
}
