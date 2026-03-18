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
}
