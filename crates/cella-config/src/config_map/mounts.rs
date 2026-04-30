use std::path::Path;

use cella_backend::MountConfig;

pub(super) fn map_workspace_mount(
    config: &serde_json::Value,
    workspace_root: &Path,
    workspace_folder: &str,
    consistency: Option<&str>,
) -> Option<MountConfig> {
    if let Some(mount_str) = config.get("workspaceMount").and_then(|v| v.as_str()) {
        if mount_str.is_empty() {
            return None; // Explicitly disabled
        }
        return parse_mount_string(mount_str);
    }

    // Default workspace mount — consistency is skipped on Linux (Podman
    // rejects it; native Docker ignores it).
    Some(MountConfig {
        mount_type: "bind".to_string(),
        source: workspace_root
            .canonicalize()
            .unwrap_or_else(|_| workspace_root.to_path_buf())
            .to_string_lossy()
            .to_string(),
        target: workspace_folder.to_string(),
        consistency: if cfg!(target_os = "linux") {
            None
        } else {
            Some(consistency.unwrap_or("cached").to_string())
        },
        read_only: false,
        external: false,
    })
}

pub fn map_additional_mounts(config: &serde_json::Value) -> Vec<MountConfig> {
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
                let read_only = obj
                    .get("readOnly")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);

                if target.is_empty() {
                    return None;
                }

                Some(MountConfig {
                    mount_type,
                    source,
                    target,
                    consistency: None,
                    read_only,
                    external: false,
                })
            }
            _ => None,
        })
        .collect()
}

pub fn parse_mount_string(s: &str) -> Option<MountConfig> {
    let mut mount_type = "bind".to_string();
    let mut source = String::new();
    let mut target = String::new();
    let mut consistency = None;
    let mut read_only = false;
    let mut external = false;

    for part in s.split(',') {
        let trimmed = part.trim();
        if let Some((key, value)) = trimmed.split_once('=') {
            let value = value.trim();
            match key.trim() {
                "type" => mount_type = value.to_string(),
                "source" | "src" => source = value.to_string(),
                "target" | "dst" | "destination" => target = value.to_string(),
                "consistency" => consistency = Some(value.to_string()),
                "external" => external = value == "true",
                _ => {}
            }
        } else {
            // Bare token (no `=`): handle read-only flags.
            match trimmed {
                "ro" | "readonly" => read_only = true,
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
        read_only,
        external,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_mount_string_with_src_dst_aliases() {
        let mount = parse_mount_string("type=volume,src=myvolume,dst=/data").unwrap();
        assert_eq!(mount.mount_type, "volume");
        assert_eq!(mount.source, "myvolume");
        assert_eq!(mount.target, "/data");
    }

    #[test]
    fn parse_mount_string_empty_returns_none() {
        assert!(parse_mount_string("").is_none());
    }

    #[test]
    fn parse_mount_string_trims_whitespace_around_equals() {
        let mount = parse_mount_string("type = bind, source = /a, target = /b").unwrap();
        assert_eq!(
            mount.mount_type, "bind",
            "leading space around '=' must not leave ' bind'"
        );
        assert_eq!(mount.source, "/a");
        assert_eq!(mount.target, "/b");
    }

    #[test]
    fn map_workspace_mount_explicitly_disabled() {
        let config = json!({"workspaceMount": ""});
        let result = map_workspace_mount(&config, Path::new("/src"), "/workspaces/proj", None);
        assert!(result.is_none());
    }

    #[test]
    fn map_workspace_mount_custom() {
        let config = json!({"workspaceMount": "type=bind,source=/host/code,target=/code"});
        let result = map_workspace_mount(&config, Path::new("/src"), "/workspaces/proj", None);
        let mount = result.unwrap();
        assert_eq!(mount.mount_type, "bind");
        assert_eq!(mount.source, "/host/code");
        assert_eq!(mount.target, "/code");
    }

    #[test]
    fn map_additional_mounts_mixed_formats() {
        let config = json!({
            "mounts": [
                "type=volume,source=vol1,target=/vol1",
                {"type": "bind", "source": "/host", "target": "/container"}
            ]
        });
        let mounts = map_additional_mounts(&config);
        assert_eq!(mounts.len(), 2);
        assert_eq!(mounts[0].mount_type, "volume");
        assert_eq!(mounts[0].target, "/vol1");
        assert_eq!(mounts[1].mount_type, "bind");
        assert_eq!(mounts[1].source, "/host");
        assert_eq!(mounts[1].target, "/container");
    }

    #[test]
    fn parse_mount_string_with_ro_bare_token() {
        let mount = parse_mount_string("type=bind,source=/a,target=/b,ro").unwrap();
        assert_eq!(mount.mount_type, "bind");
        assert_eq!(mount.source, "/a");
        assert_eq!(mount.target, "/b");
        assert!(mount.read_only, "ro bare token must set read_only=true");
    }

    #[test]
    fn parse_mount_string_with_readonly_bare_token() {
        let mount = parse_mount_string("type=bind,source=/a,target=/b,readonly").unwrap();
        assert!(
            mount.read_only,
            "readonly bare token must set read_only=true"
        );
    }

    #[test]
    fn parse_mount_string_read_only_defaults_false() {
        let mount = parse_mount_string("type=bind,source=/a,target=/b").unwrap();
        assert!(
            !mount.read_only,
            "no ro token → read_only must default to false"
        );
    }

    #[test]
    fn map_additional_mounts_honors_read_only_object() {
        let config = json!({
            "mounts": [
                {"type": "bind", "source": "/host", "target": "/container", "readOnly": true}
            ]
        });
        let mounts = map_additional_mounts(&config);
        assert_eq!(mounts.len(), 1);
        assert!(
            mounts[0].read_only,
            "readOnly:true must propagate to MountConfig"
        );
    }

    #[test]
    fn map_additional_mounts_read_only_defaults_false_object() {
        let config = json!({
            "mounts": [
                {"type": "bind", "source": "/host", "target": "/container"}
            ]
        });
        let mounts = map_additional_mounts(&config);
        assert_eq!(mounts.len(), 1);
        assert!(
            !mounts[0].read_only,
            "absent readOnly must default to false"
        );
    }
}
