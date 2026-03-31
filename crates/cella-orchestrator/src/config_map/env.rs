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


#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn map_container_env_basic() {
        let config = json!({"containerEnv": {"FOO": "bar", "BAZ": "qux"}});
        let env = map_container_env(&config);
        assert_eq!(env.len(), 2);
        assert!(env.contains(&"FOO=bar".to_string()));
        assert!(env.contains(&"BAZ=qux".to_string()));
    }

    #[test]
    fn map_container_env_missing() {
        let config = json!({});
        let env = map_container_env(&config);
        assert!(env.is_empty());
    }

    #[test]
    fn map_remote_env_basic() {
        let config = json!({"remoteEnv": {"EDITOR": "vim"}});
        let env = map_remote_env(&config);
        assert_eq!(env, vec!["EDITOR=vim"]);
    }

    #[test]
    fn map_remote_env_non_string_values() {
        let config = json!({"remoteEnv": {"NUM": 42, "BOOL": true, "NULL": null}});
        let env = map_remote_env(&config);
        assert_eq!(env.len(), 3);
        for entry in &env {
            let parts: Vec<&str> = entry.splitn(2, '=').collect();
            assert_eq!(parts.len(), 2);
            match parts[0] {
                "NUM" | "BOOL" | "NULL" => assert_eq!(parts[1], ""),
                _ => panic!("unexpected key: {}", parts[0]),
            }
        }
    }
}
