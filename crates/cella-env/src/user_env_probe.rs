//! userEnvProbe spec implementation.
//!
//! Generates the command to probe the container user's environment
//! and parses the null-delimited output.

use std::collections::HashMap;

/// Generate the shell command to probe the user's environment.
///
/// Returns `None` if `probe_type` is `"none"`.
///
/// # Arguments
/// * `probe_type` - The `userEnvProbe` config value
/// * `shell` - The user's shell (e.g., "/bin/bash")
pub fn probe_command(probe_type: &str, shell: &str) -> Option<Vec<String>> {
    let flags = match probe_type {
        "none" => return None,
        "loginShell" => "-l",
        "interactiveShell" => "-i",
        // Default per spec (loginInteractiveShell, empty, or unknown)
        _ => "-li",
    };

    Some(vec![
        shell.to_string(),
        flags.to_string(),
        "-c".to_string(),
        "env -0".to_string(),
    ])
}

/// Parse null-delimited environment output into a map.
pub fn parse_probed_env(output: &str) -> HashMap<String, String> {
    output
        .split('\0')
        .filter(|s| !s.is_empty())
        .filter_map(|entry| {
            let (key, value) = entry.split_once('=')?;
            Some((key.to_string(), value.to_string()))
        })
        .collect()
}

/// Merge probed environment with `remoteEnv` from config.
///
/// `remote_env` values override `probed` values.
#[allow(clippy::implicit_hasher)]
pub fn merge_env(probed: &HashMap<String, String>, remote_env: &[String]) -> Vec<String> {
    let mut merged = probed.clone();

    for entry in remote_env {
        if let Some((key, value)) = entry.split_once('=') {
            merged.insert(key.to_string(), value.to_string());
        }
    }

    merged
        .into_iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_command_none() {
        assert!(probe_command("none", "/bin/bash").is_none());
    }

    #[test]
    fn probe_command_login_shell() {
        let cmd = probe_command("loginShell", "/bin/bash").unwrap();
        assert_eq!(cmd, vec!["/bin/bash", "-l", "-c", "env -0"]);
    }

    #[test]
    fn probe_command_interactive_shell() {
        let cmd = probe_command("interactiveShell", "/bin/zsh").unwrap();
        assert_eq!(cmd, vec!["/bin/zsh", "-i", "-c", "env -0"]);
    }

    #[test]
    fn probe_command_default() {
        let cmd = probe_command("loginInteractiveShell", "/bin/bash").unwrap();
        assert_eq!(cmd, vec!["/bin/bash", "-li", "-c", "env -0"]);
    }

    #[test]
    fn probe_command_empty_defaults() {
        let cmd = probe_command("", "/bin/bash").unwrap();
        assert_eq!(cmd, vec!["/bin/bash", "-li", "-c", "env -0"]);
    }

    #[test]
    fn parse_env_output() {
        let output = "HOME=/home/user\0PATH=/usr/bin:/bin\0SHELL=/bin/bash\0";
        let env = parse_probed_env(output);
        assert_eq!(env.get("HOME"), Some(&"/home/user".to_string()));
        assert_eq!(env.get("PATH"), Some(&"/usr/bin:/bin".to_string()));
        assert_eq!(env.get("SHELL"), Some(&"/bin/bash".to_string()));
        assert_eq!(env.len(), 3);
    }

    #[test]
    fn parse_empty_output() {
        let env = parse_probed_env("");
        assert!(env.is_empty());
    }

    #[test]
    fn parse_entry_without_equals() {
        // Malformed entries should be skipped
        let output = "GOOD=value\0BADENTRY\0ALSO_GOOD=val2\0";
        let env = parse_probed_env(output);
        assert_eq!(env.len(), 2);
        assert!(env.contains_key("GOOD"));
        assert!(env.contains_key("ALSO_GOOD"));
    }

    #[test]
    fn merge_remote_env_overrides() {
        let mut probed = HashMap::new();
        probed.insert("PATH".to_string(), "/usr/bin".to_string());
        probed.insert("HOME".to_string(), "/home/user".to_string());

        let remote_env = vec!["PATH=/custom/bin:/usr/bin".to_string()];
        let merged = merge_env(&probed, &remote_env);

        let merged_map: HashMap<String, String> = merged
            .into_iter()
            .filter_map(|e| {
                let (k, v) = e.split_once('=')?;
                Some((k.to_string(), v.to_string()))
            })
            .collect();

        assert_eq!(
            merged_map.get("PATH"),
            Some(&"/custom/bin:/usr/bin".to_string())
        );
        assert_eq!(merged_map.get("HOME"), Some(&"/home/user".to_string()));
    }
}
