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
];

/// Keys whose array values should be concatenated (duplicates allowed).
const CONCAT_ARRAY_KEYS: &[&str] = &["mounts", "runArgs", "overrideFeatureInstallOrder"];

/// Keys whose array values form a union (no duplicates).
const UNION_ARRAY_KEYS: &[&str] = &["forwardPorts", "capAdd", "securityOpt"];

/// Keys with boolean-OR semantics: any `true` wins.
const BOOLEAN_OR_KEYS: &[&str] = &["init", "privileged"];

/// Merge two devcontainer config layers. `base` is modified in place.
/// `overlay` values take precedence.
pub fn layers(base: &mut serde_json::Value, overlay: &serde_json::Value) {
    let (Some(base_obj), Some(overlay_obj)) = (base.as_object_mut(), overlay.as_object()) else {
        // If either isn't an object, overlay wins entirely
        *base = overlay.clone();
        return;
    };

    for (key, overlay_value) in overlay_obj {
        let key_str = key.as_str();

        // Boolean OR: any true wins
        if BOOLEAN_OR_KEYS.contains(&key_str) {
            let base_bool = base_obj.get(key).and_then(serde_json::Value::as_bool);
            let overlay_bool = overlay_value.as_bool();
            if let (Some(b), Some(o)) = (base_bool, overlay_bool) {
                base_obj.insert(key.clone(), serde_json::Value::Bool(b || o));
                continue;
            }
        }

        // hostRequirements: per-key max value
        if key_str == "hostRequirements"
            && let Some(base_value) = base_obj.get_mut(key)
        {
            merge_host_requirements(base_value, overlay_value);
            continue;
        }

        // Deep-merge object keys
        if DEEP_MERGE_KEYS.contains(&key_str)
            && let Some(base_value) = base_obj.get_mut(key)
        {
            deep_merge(base_value, overlay_value);
            continue;
        }

        // Union arrays (concat + dedup)
        if UNION_ARRAY_KEYS.contains(&key_str)
            && let (Some(base_value), Some(overlay_arr)) =
                (base_obj.get_mut(key), overlay_value.as_array())
            && let Some(base_arr) = base_value.as_array_mut()
        {
            for item in overlay_arr {
                if !base_arr.contains(item) {
                    base_arr.push(item.clone());
                }
            }
            continue;
        }

        // Concat arrays (duplicates allowed)
        if CONCAT_ARRAY_KEYS.contains(&key_str)
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

/// Parse a memory string like "4gb", "512mb" into bytes for comparison.
fn parse_memory_bytes(s: &str) -> Option<u64> {
    let s = s.trim().to_ascii_lowercase();
    let suffixes: &[(&str, u64)] = &[
        ("tb", 1_099_511_627_776),
        ("gb", 1_073_741_824),
        ("mb", 1_048_576),
        ("kb", 1024),
    ];
    let (num_part, multiplier) = suffixes
        .iter()
        .find_map(|(suffix, mult)| s.strip_suffix(suffix).map(|n| (n, *mult)))
        .unwrap_or((s.as_str(), 1));
    num_part.trim().parse::<u64>().ok().map(|n| n * multiplier)
}

/// Merge `hostRequirements` using max-value semantics per key.
fn merge_host_requirements(base: &mut serde_json::Value, overlay: &serde_json::Value) {
    let (Some(base_obj), Some(overlay_obj)) = (base.as_object_mut(), overlay.as_object()) else {
        *base = overlay.clone();
        return;
    };

    for (key, overlay_value) in overlay_obj {
        match base_obj.get(key) {
            Some(base_value) => {
                // Try numeric comparison first
                if let (Some(b), Some(o)) = (base_value.as_u64(), overlay_value.as_u64()) {
                    if o > b {
                        base_obj.insert(key.clone(), overlay_value.clone());
                    }
                } else if let (Some(b), Some(o)) = (base_value.as_f64(), overlay_value.as_f64()) {
                    if o > b {
                        base_obj.insert(key.clone(), overlay_value.clone());
                    }
                }
                // Try memory-string comparison (e.g. "4gb" vs "2gb")
                else if let (Some(b_str), Some(o_str)) =
                    (base_value.as_str(), overlay_value.as_str())
                    && let (Some(b_bytes), Some(o_bytes)) =
                        (parse_memory_bytes(b_str), parse_memory_bytes(o_str))
                {
                    if o_bytes > b_bytes {
                        base_obj.insert(key.clone(), overlay_value.clone());
                    }
                } else {
                    // Fallback: overlay wins
                    base_obj.insert(key.clone(), overlay_value.clone());
                }
            }
            None => {
                base_obj.insert(key.clone(), overlay_value.clone());
            }
        }
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
        layers(&mut base, &overlay);
        assert_eq!(base["name"], "overlay");
        assert_eq!(base["image"], "ubuntu");
    }

    #[test]
    fn test_deep_merge_features() {
        let mut base = json!({"features": {"a": {}, "b": {"version": "1"}}});
        let overlay = json!({"features": {"b": {"version": "2"}, "c": {}}});
        layers(&mut base, &overlay);
        assert_eq!(base["features"]["a"], json!({}));
        assert_eq!(base["features"]["b"]["version"], "2");
        assert_eq!(base["features"]["c"], json!({}));
    }

    #[test]
    fn test_concat_arrays() {
        let mut base = json!({"forwardPorts": [3000]});
        let overlay = json!({"forwardPorts": [8080]});
        layers(&mut base, &overlay);
        assert_eq!(base["forwardPorts"], json!([3000, 8080]));
    }

    #[test]
    fn test_lifecycle_override() {
        let mut base = json!({"postCreateCommand": "echo base"});
        let overlay = json!({"postCreateCommand": "echo overlay"});
        layers(&mut base, &overlay);
        assert_eq!(base["postCreateCommand"], "echo overlay");
    }

    #[test]
    fn test_empty_overlay() {
        let mut base = json!({"name": "base", "image": "ubuntu"});
        let overlay = json!({});
        layers(&mut base, &overlay);
        assert_eq!(base, json!({"name": "base", "image": "ubuntu"}));
    }

    #[test]
    fn test_empty_base() {
        let mut base = json!({});
        let overlay = json!({"name": "overlay", "image": "alpine"});
        layers(&mut base, &overlay);
        assert_eq!(base, json!({"name": "overlay", "image": "alpine"}));
    }

    #[test]
    fn test_null_value_override() {
        let mut base = json!({"name": "base"});
        let overlay = json!({"name": null});
        layers(&mut base, &overlay);
        assert_eq!(base["name"], json!(null));
    }

    #[test]
    fn test_unknown_keys_passthrough() {
        let mut base = json!({"foo": "bar", "baz": 1});
        let overlay = json!({"foo": "qux"});
        layers(&mut base, &overlay);
        assert_eq!(base["foo"], "qux");
        assert_eq!(base["baz"], 1);
    }

    #[test]
    fn test_deep_merge_container_env() {
        let mut base = json!({"containerEnv": {"A": "1", "B": "2"}});
        let overlay = json!({"containerEnv": {"B": "3", "C": "4"}});
        layers(&mut base, &overlay);
        assert_eq!(base["containerEnv"]["A"], "1");
        assert_eq!(base["containerEnv"]["B"], "3");
        assert_eq!(base["containerEnv"]["C"], "4");
    }

    #[test]
    fn test_deep_merge_remote_env() {
        let mut base = json!({"remoteEnv": {"PATH": "/usr/bin"}});
        let overlay = json!({"remoteEnv": {"HOME": "/home/user"}});
        layers(&mut base, &overlay);
        assert_eq!(base["remoteEnv"]["PATH"], "/usr/bin");
        assert_eq!(base["remoteEnv"]["HOME"], "/home/user");
    }

    #[test]
    fn test_deep_merge_customizations() {
        let mut base = json!({"customizations": {"vscode": {"extensions": ["ext1"]}}});
        let overlay = json!({"customizations": {"vscode": {"settings": {}}, "other": {}}});
        layers(&mut base, &overlay);
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
        layers(&mut base, &overlay);
        assert_eq!(base["portsAttributes"]["3000"]["label"], "app");
        assert_eq!(base["portsAttributes"]["8080"]["label"], "api");
    }

    #[test]
    fn test_concat_mounts() {
        let mut base = json!({"mounts": ["source=a,target=/a"]});
        let overlay = json!({"mounts": ["source=b,target=/b"]});
        layers(&mut base, &overlay);
        assert_eq!(
            base["mounts"],
            json!(["source=a,target=/a", "source=b,target=/b"])
        );
    }

    #[test]
    fn test_concat_cap_add() {
        let mut base = json!({"capAdd": ["SYS_PTRACE"]});
        let overlay = json!({"capAdd": ["NET_ADMIN"]});
        layers(&mut base, &overlay);
        assert_eq!(base["capAdd"], json!(["SYS_PTRACE", "NET_ADMIN"]));
    }

    #[test]
    fn test_concat_run_args() {
        let mut base = json!({"runArgs": ["--init"]});
        let overlay = json!({"runArgs": ["--privileged"]});
        layers(&mut base, &overlay);
        assert_eq!(base["runArgs"], json!(["--init", "--privileged"]));
    }

    #[test]
    fn test_nested_deep_merge_preserves_siblings() {
        let mut base = json!({"features": {"a": {"x": 1, "y": 2}}});
        let overlay = json!({"features": {"a": {"y": 3}}});
        layers(&mut base, &overlay);
        assert_eq!(base["features"]["a"]["x"], 1);
        assert_eq!(base["features"]["a"]["y"], 3);
    }

    #[test]
    fn test_non_object_overlay_replaces() {
        let mut base = json!({"name": "base"});
        let overlay = json!("just a string");
        layers(&mut base, &overlay);
        assert_eq!(base, json!("just a string"));
    }

    #[test]
    fn test_concat_only_when_both_arrays() {
        let mut base = json!({"forwardPorts": "not an array"});
        let overlay = json!({"forwardPorts": [8080]});
        layers(&mut base, &overlay);
        assert_eq!(base["forwardPorts"], json!([8080]));
    }

    #[test]
    fn test_deep_merge_base_key_missing() {
        let mut base = json!({"name": "test"});
        let overlay = json!({"features": {"a": {}}});
        layers(&mut base, &overlay);
        assert_eq!(base["features"], json!({"a": {}}));
    }

    // --- Spec compliance tests ---
    // Reference: https://containers.dev/implementors/spec/#merge-logic

    #[test]
    fn spec_init_boolean_or_any_true_wins() {
        let mut base = json!({"init": false});
        let overlay = json!({"init": true});
        layers(&mut base, &overlay);
        assert_eq!(base["init"], true, "init: any true should win");
    }

    #[test]
    fn spec_init_boolean_or_both_false() {
        let mut base = json!({"init": false});
        let overlay = json!({"init": false});
        layers(&mut base, &overlay);
        assert_eq!(base["init"], false);
    }

    #[test]
    fn spec_privileged_boolean_or_any_true_wins() {
        let mut base = json!({"privileged": true});
        let overlay = json!({"privileged": false});
        layers(&mut base, &overlay);
        assert_eq!(
            base["privileged"], true,
            "privileged: any true should win (boolean OR)"
        );
    }

    #[test]
    fn spec_cap_add_union_without_duplicates() {
        let mut base = json!({"capAdd": ["SYS_PTRACE", "NET_ADMIN"]});
        let overlay = json!({"capAdd": ["NET_ADMIN", "SYS_ADMIN"]});
        layers(&mut base, &overlay);

        let caps = base["capAdd"].as_array().unwrap();
        let cap_strs: Vec<&str> = caps.iter().filter_map(|v| v.as_str()).collect();

        assert!(cap_strs.contains(&"SYS_PTRACE"));
        assert!(cap_strs.contains(&"NET_ADMIN"));
        assert!(cap_strs.contains(&"SYS_ADMIN"));

        let mut sorted = cap_strs.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(cap_strs.len(), sorted.len(), "capAdd should have no duplicates");
    }

    #[test]
    fn spec_security_opt_union_without_duplicates() {
        let mut base = json!({"securityOpt": ["seccomp=unconfined"]});
        let overlay = json!({"securityOpt": ["seccomp=unconfined", "apparmor=unconfined"]});
        layers(&mut base, &overlay);

        let opts = base["securityOpt"].as_array().unwrap();
        let opt_strs: Vec<&str> = opts.iter().filter_map(|v| v.as_str()).collect();

        assert!(opt_strs.contains(&"seccomp=unconfined"));
        assert!(opt_strs.contains(&"apparmor=unconfined"));

        let mut sorted = opt_strs.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(opt_strs.len(), sorted.len(), "securityOpt should have no duplicates");
    }

    #[test]
    fn spec_forward_ports_union_without_duplicates() {
        let mut base = json!({"forwardPorts": [3000, 8080]});
        let overlay = json!({"forwardPorts": [8080, 5432]});
        layers(&mut base, &overlay);

        let ports = base["forwardPorts"].as_array().unwrap();
        let port_nums: Vec<i64> = ports.iter().filter_map(serde_json::Value::as_i64).collect();

        assert!(port_nums.contains(&3000));
        assert!(port_nums.contains(&8080));
        assert!(port_nums.contains(&5432));

        let mut sorted = port_nums.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(port_nums.len(), sorted.len(), "forwardPorts should have no duplicates");
    }

    #[test]
    fn spec_mounts_collected_list() {
        let mut base = json!({"mounts": ["source=a,target=/a"]});
        let overlay = json!({"mounts": ["source=b,target=/b"]});
        layers(&mut base, &overlay);

        let mounts = base["mounts"].as_array().unwrap();
        assert_eq!(mounts.len(), 2);
    }

    #[test]
    fn spec_lifecycle_commands_last_wins_in_layer_merge() {
        let mut base = json!({"postCreateCommand": "echo base"});
        let overlay = json!({"postCreateCommand": "echo overlay"});
        layers(&mut base, &overlay);
        assert_eq!(base["postCreateCommand"], "echo overlay");
    }

    #[test]
    fn spec_wait_for_last_value_wins() {
        let mut base = json!({"waitFor": "onCreateCommand"});
        let overlay = json!({"waitFor": "postCreateCommand"});
        layers(&mut base, &overlay);
        assert_eq!(base["waitFor"], "postCreateCommand");
    }

    #[test]
    fn spec_remote_user_last_value_wins() {
        let mut base = json!({"remoteUser": "root"});
        let overlay = json!({"remoteUser": "vscode"});
        layers(&mut base, &overlay);
        assert_eq!(base["remoteUser"], "vscode");
    }

    #[test]
    fn spec_container_user_last_value_wins() {
        let mut base = json!({"containerUser": "root"});
        let overlay = json!({"containerUser": "dev"});
        layers(&mut base, &overlay);
        assert_eq!(base["containerUser"], "dev");
    }

    #[test]
    fn spec_user_env_probe_last_value_wins() {
        let mut base = json!({"userEnvProbe": "loginShell"});
        let overlay = json!({"userEnvProbe": "interactiveShell"});
        layers(&mut base, &overlay);
        assert_eq!(base["userEnvProbe"], "interactiveShell");
    }

    #[test]
    fn spec_override_command_last_value_wins() {
        let mut base = json!({"overrideCommand": true});
        let overlay = json!({"overrideCommand": false});
        layers(&mut base, &overlay);
        assert_eq!(base["overrideCommand"], false);
    }

    #[test]
    fn spec_shutdown_action_last_value_wins() {
        let mut base = json!({"shutdownAction": "none"});
        let overlay = json!({"shutdownAction": "stopContainer"});
        layers(&mut base, &overlay);
        assert_eq!(base["shutdownAction"], "stopContainer");
    }

    #[test]
    fn spec_update_remote_user_uid_last_value_wins() {
        let mut base = json!({"updateRemoteUserUID": true});
        let overlay = json!({"updateRemoteUserUID": false});
        layers(&mut base, &overlay);
        assert_eq!(base["updateRemoteUserUID"], false);
    }

    #[test]
    fn spec_remote_env_per_key_last_wins() {
        let mut base = json!({"remoteEnv": {"A": "1", "B": "2"}});
        let overlay = json!({"remoteEnv": {"B": "3", "C": "4"}});
        layers(&mut base, &overlay);
        assert_eq!(base["remoteEnv"]["A"], "1");
        assert_eq!(base["remoteEnv"]["B"], "3");
        assert_eq!(base["remoteEnv"]["C"], "4");
    }

    #[test]
    fn spec_container_env_per_key_last_wins() {
        let mut base = json!({"containerEnv": {"X": "old"}});
        let overlay = json!({"containerEnv": {"X": "new", "Y": "added"}});
        layers(&mut base, &overlay);
        assert_eq!(base["containerEnv"]["X"], "new");
        assert_eq!(base["containerEnv"]["Y"], "added");
    }

    #[test]
    fn spec_ports_attributes_per_port_last_wins() {
        let mut base = json!({"portsAttributes": {"3000": {"label": "app"}}});
        let overlay = json!({"portsAttributes": {"3000": {"label": "frontend"}, "8080": {"label": "api"}}});
        layers(&mut base, &overlay);
        assert_eq!(base["portsAttributes"]["3000"]["label"], "frontend");
        assert_eq!(base["portsAttributes"]["8080"]["label"], "api");
    }

    #[test]
    fn spec_host_requirements_max_value_wins() {
        let mut base = json!({"hostRequirements": {"cpus": 2, "memory": "4gb"}});
        let overlay = json!({"hostRequirements": {"cpus": 4, "memory": "2gb"}});
        layers(&mut base, &overlay);
        assert_eq!(base["hostRequirements"]["cpus"], 4, "cpus should use max value");
        assert_eq!(
            base["hostRequirements"]["memory"], "4gb",
            "memory should use max value (4gb > 2gb)"
        );
    }

    #[test]
    fn spec_customizations_deep_merge() {
        let mut base = json!({"customizations": {"vscode": {"extensions": ["ext1"]}}});
        let overlay = json!({"customizations": {"vscode": {"settings": {}}, "cella": {"key": "val"}}});
        layers(&mut base, &overlay);
        assert_eq!(base["customizations"]["vscode"]["extensions"], json!(["ext1"]));
        assert_eq!(base["customizations"]["vscode"]["settings"], json!({}));
        assert_eq!(base["customizations"]["cella"]["key"], "val");
    }

    #[test]
    fn spec_features_deep_merge() {
        let mut base = json!({"features": {"a": {"version": "1"}, "b": {}}});
        let overlay = json!({"features": {"a": {"version": "2"}, "c": {}}});
        layers(&mut base, &overlay);
        assert_eq!(base["features"]["a"]["version"], "2");
        assert_eq!(base["features"]["b"], json!({}));
        assert_eq!(base["features"]["c"], json!({}));
    }

    #[test]
    fn spec_other_ports_attributes_last_wins() {
        let mut base = json!({"otherPortsAttributes": {"onAutoForward": "notify"}});
        let overlay = json!({"otherPortsAttributes": {"onAutoForward": "silent"}});
        layers(&mut base, &overlay);
        assert_eq!(base["otherPortsAttributes"]["onAutoForward"], "silent");
    }
}
