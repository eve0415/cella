pub(super) fn map_container_env(config: &serde_json::Value) -> Vec<String> {
    let Some(env_obj) = config.get("containerEnv").and_then(|v| v.as_object()) else {
        return Vec::new();
    };

    env_obj
        .iter()
        .map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or("")))
        .collect()
}

pub(super) fn map_remote_env(config: &serde_json::Value) -> Vec<String> {
    let Some(env_obj) = config.get("remoteEnv").and_then(|v| v.as_object()) else {
        return Vec::new();
    };

    env_obj
        .iter()
        .map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or("")))
        .collect()
}
