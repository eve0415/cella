//! Config layer merging: merge multiple `serde_json::Values` representing
//! devcontainer.json layers.
//!
//! Merge rules:
//! - Scalars: later layer wins
//! - Objects (features, containerEnv, customizations): deep merge
//! - Arrays (forwardPorts, mounts): concatenate
//! - Lifecycle commands: later layer wins entirely

/// Keys whose object values should be deep-merged.
const DEEP_MERGE_KEYS: &[&str] = &[
    "features",
    "containerEnv",
    "remoteEnv",
    "customizations",
    "portsAttributes",
    "otherPortsAttributes",
    "hostRequirements",
];

/// Keys whose array values should be concatenated.
const CONCAT_ARRAY_KEYS: &[&str] = &[
    "forwardPorts",
    "mounts",
    "capAdd",
    "securityOpt",
    "runArgs",
    "overrideFeatureInstallOrder",
];

/// Merge two devcontainer config layers. `base` is modified in place.
/// `overlay` values take precedence.
pub fn merge_layers(base: &mut serde_json::Value, overlay: &serde_json::Value) {
    let (Some(base_obj), Some(overlay_obj)) = (base.as_object_mut(), overlay.as_object()) else {
        // If either isn't an object, overlay wins entirely
        *base = overlay.clone();
        return;
    };

    for (key, overlay_value) in overlay_obj {
        if DEEP_MERGE_KEYS.contains(&key.as_str())
            && let Some(base_value) = base_obj.get_mut(key)
        {
            deep_merge(base_value, overlay_value);
            continue;
        }

        if CONCAT_ARRAY_KEYS.contains(&key.as_str())
            && let (Some(base_value), Some(overlay_arr)) =
                (base_obj.get_mut(key), overlay_value.as_array())
            && let Some(base_arr) = base_value.as_array_mut()
        {
            base_arr.extend(overlay_arr.iter().cloned());
            continue;
        }

        // Default: overlay wins
        base_obj.insert(key.clone(), overlay_value.clone());
    }
}

fn deep_merge(base: &mut serde_json::Value, overlay: &serde_json::Value) {
    match (base.as_object_mut(), overlay.as_object()) {
        (Some(base_obj), Some(overlay_obj)) => {
            for (key, overlay_value) in overlay_obj {
                if let Some(base_value) = base_obj.get_mut(key) {
                    deep_merge(base_value, overlay_value);
                } else {
                    base_obj.insert(key.clone(), overlay_value.clone());
                }
            }
        }
        _ => {
            *base = overlay.clone();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_scalar_override() {
        let mut base = json!({"name": "base", "image": "ubuntu"});
        let overlay = json!({"name": "overlay"});
        merge_layers(&mut base, &overlay);
        assert_eq!(base["name"], "overlay");
        assert_eq!(base["image"], "ubuntu");
    }

    #[test]
    fn test_deep_merge_features() {
        let mut base = json!({"features": {"a": {}, "b": {"version": "1"}}});
        let overlay = json!({"features": {"b": {"version": "2"}, "c": {}}});
        merge_layers(&mut base, &overlay);
        assert_eq!(base["features"]["a"], json!({}));
        assert_eq!(base["features"]["b"]["version"], "2");
        assert_eq!(base["features"]["c"], json!({}));
    }

    #[test]
    fn test_concat_arrays() {
        let mut base = json!({"forwardPorts": [3000]});
        let overlay = json!({"forwardPorts": [8080]});
        merge_layers(&mut base, &overlay);
        assert_eq!(base["forwardPorts"], json!([3000, 8080]));
    }

    #[test]
    fn test_lifecycle_override() {
        let mut base = json!({"postCreateCommand": "echo base"});
        let overlay = json!({"postCreateCommand": "echo overlay"});
        merge_layers(&mut base, &overlay);
        assert_eq!(base["postCreateCommand"], "echo overlay");
    }

    #[test]
    fn test_empty_overlay() {
        let mut base = json!({"name": "base", "image": "ubuntu"});
        let overlay = json!({});
        merge_layers(&mut base, &overlay);
        assert_eq!(base, json!({"name": "base", "image": "ubuntu"}));
    }

    #[test]
    fn test_empty_base() {
        let mut base = json!({});
        let overlay = json!({"name": "overlay", "image": "alpine"});
        merge_layers(&mut base, &overlay);
        assert_eq!(base, json!({"name": "overlay", "image": "alpine"}));
    }

    #[test]
    fn test_null_value_override() {
        let mut base = json!({"name": "base"});
        let overlay = json!({"name": null});
        merge_layers(&mut base, &overlay);
        assert_eq!(base["name"], json!(null));
    }

    #[test]
    fn test_unknown_keys_passthrough() {
        let mut base = json!({"foo": "bar", "baz": 1});
        let overlay = json!({"foo": "qux"});
        merge_layers(&mut base, &overlay);
        assert_eq!(base["foo"], "qux");
        assert_eq!(base["baz"], 1);
    }

    #[test]
    fn test_deep_merge_container_env() {
        let mut base = json!({"containerEnv": {"A": "1", "B": "2"}});
        let overlay = json!({"containerEnv": {"B": "3", "C": "4"}});
        merge_layers(&mut base, &overlay);
        assert_eq!(base["containerEnv"]["A"], "1");
        assert_eq!(base["containerEnv"]["B"], "3");
        assert_eq!(base["containerEnv"]["C"], "4");
    }

    #[test]
    fn test_deep_merge_remote_env() {
        let mut base = json!({"remoteEnv": {"PATH": "/usr/bin"}});
        let overlay = json!({"remoteEnv": {"HOME": "/home/user"}});
        merge_layers(&mut base, &overlay);
        assert_eq!(base["remoteEnv"]["PATH"], "/usr/bin");
        assert_eq!(base["remoteEnv"]["HOME"], "/home/user");
    }

    #[test]
    fn test_deep_merge_customizations() {
        let mut base = json!({"customizations": {"vscode": {"extensions": ["ext1"]}}});
        let overlay = json!({"customizations": {"vscode": {"settings": {}}, "other": {}}});
        merge_layers(&mut base, &overlay);
        assert_eq!(
            base["customizations"]["vscode"]["extensions"],
            json!(["ext1"])
        );
        assert_eq!(base["customizations"]["vscode"]["settings"], json!({}));
        assert_eq!(base["customizations"]["other"], json!({}));
    }

    #[test]
    fn test_deep_merge_ports_attributes() {
        let mut base = json!({"portsAttributes": {"3000": {"label": "app"}}});
        let overlay = json!({"portsAttributes": {"8080": {"label": "api"}}});
        merge_layers(&mut base, &overlay);
        assert_eq!(base["portsAttributes"]["3000"]["label"], "app");
        assert_eq!(base["portsAttributes"]["8080"]["label"], "api");
    }

    #[test]
    fn test_concat_mounts() {
        let mut base = json!({"mounts": ["source=a,target=/a"]});
        let overlay = json!({"mounts": ["source=b,target=/b"]});
        merge_layers(&mut base, &overlay);
        assert_eq!(
            base["mounts"],
            json!(["source=a,target=/a", "source=b,target=/b"])
        );
    }

    #[test]
    fn test_concat_cap_add() {
        let mut base = json!({"capAdd": ["SYS_PTRACE"]});
        let overlay = json!({"capAdd": ["NET_ADMIN"]});
        merge_layers(&mut base, &overlay);
        assert_eq!(base["capAdd"], json!(["SYS_PTRACE", "NET_ADMIN"]));
    }

    #[test]
    fn test_concat_run_args() {
        let mut base = json!({"runArgs": ["--init"]});
        let overlay = json!({"runArgs": ["--privileged"]});
        merge_layers(&mut base, &overlay);
        assert_eq!(base["runArgs"], json!(["--init", "--privileged"]));
    }

    #[test]
    fn test_nested_deep_merge_preserves_siblings() {
        let mut base = json!({"features": {"a": {"x": 1, "y": 2}}});
        let overlay = json!({"features": {"a": {"y": 3}}});
        merge_layers(&mut base, &overlay);
        assert_eq!(base["features"]["a"]["x"], 1);
        assert_eq!(base["features"]["a"]["y"], 3);
    }

    #[test]
    fn test_non_object_overlay_replaces() {
        let mut base = json!({"name": "base"});
        let overlay = json!("just a string");
        merge_layers(&mut base, &overlay);
        assert_eq!(base, json!("just a string"));
    }

    #[test]
    fn test_concat_only_when_both_arrays() {
        let mut base = json!({"forwardPorts": "not an array"});
        let overlay = json!({"forwardPorts": [8080]});
        merge_layers(&mut base, &overlay);
        assert_eq!(base["forwardPorts"], json!([8080]));
    }

    #[test]
    fn test_deep_merge_base_key_missing() {
        let mut base = json!({"name": "test"});
        let overlay = json!({"features": {"a": {}}});
        merge_layers(&mut base, &overlay);
        assert_eq!(base["features"], json!({"a": {}}));
    }
}
