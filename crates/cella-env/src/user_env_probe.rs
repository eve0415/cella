//! userEnvProbe spec implementation.
//!
//! Generates the command to probe the container user's environment
//! and parses the null-delimited output.

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "cli", derive(clap::ValueEnum))]
#[serde(rename_all = "camelCase")]
pub enum UserEnvProbe {
    None,
    LoginShell,
    InteractiveShell,
    #[default]
    LoginInteractiveShell,
}

impl UserEnvProbe {
    pub const fn shell_flags(self) -> Option<&'static str> {
        match self {
            Self::None => None,
            Self::LoginShell => Some("-l"),
            Self::InteractiveShell => Some("-i"),
            Self::LoginInteractiveShell => Some("-li"),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::LoginShell => "loginShell",
            Self::InteractiveShell => "interactiveShell",
            Self::LoginInteractiveShell => "loginInteractiveShell",
        }
    }
}

impl fmt::Display for UserEnvProbe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for UserEnvProbe {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "none" => Ok(Self::None),
            "loginShell" => Ok(Self::LoginShell),
            "interactiveShell" => Ok(Self::InteractiveShell),
            "loginInteractiveShell" => Ok(Self::LoginInteractiveShell),
            other => Err(format!("unknown userEnvProbe value: {other}")),
        }
    }
}

/// Generate the shell command to probe the user's environment.
///
/// Returns `None` if `probe_type` is `None`.
pub fn probe_command(probe_type: UserEnvProbe, shell: &str) -> Option<Vec<String>> {
    let flags = probe_type.shell_flags()?;
    Some(vec![
        shell.to_string(),
        flags.to_string(),
        "-c".to_string(),
        "env -0".to_string(),
    ])
}

/// Parse null-delimited environment output into a map.
///
/// Filters out `PWD` — the probed working directory is meaningless
/// (it's the probe command's cwd) and can interfere with exec sessions.
pub fn parse_probed_env(output: &str) -> HashMap<String, String> {
    output
        .split('\0')
        .filter(|s| !s.is_empty())
        .filter_map(|entry| {
            let (key, value) = entry.split_once('=')?;
            if key == "PWD" {
                return None;
            }
            Some((key.to_string(), value.to_string()))
        })
        .collect()
}

/// Merge probed environment with `remoteEnv` from config.
///
/// `remote_env` values override `probed` values.
pub fn merge_env<S: std::hash::BuildHasher>(
    probed: &HashMap<String, String, S>,
    remote_env: &[String],
) -> Vec<String> {
    let mut merged: HashMap<String, String> =
        probed.iter().map(|(k, v)| (k.clone(), v.clone())).collect();

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
        assert!(probe_command(UserEnvProbe::None, "/bin/bash").is_none());
    }

    #[test]
    fn probe_command_login_shell() {
        let cmd = probe_command(UserEnvProbe::LoginShell, "/bin/bash").unwrap();
        assert_eq!(cmd, vec!["/bin/bash", "-l", "-c", "env -0"]);
    }

    #[test]
    fn probe_command_interactive_shell() {
        let cmd = probe_command(UserEnvProbe::InteractiveShell, "/bin/zsh").unwrap();
        assert_eq!(cmd, vec!["/bin/zsh", "-i", "-c", "env -0"]);
    }

    #[test]
    fn probe_command_default() {
        let cmd = probe_command(UserEnvProbe::LoginInteractiveShell, "/bin/bash").unwrap();
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
        let output = "GOOD=value\0BADENTRY\0ALSO_GOOD=val2\0";
        let env = parse_probed_env(output);
        assert_eq!(env.len(), 2);
        assert!(env.contains_key("GOOD"));
        assert!(env.contains_key("ALSO_GOOD"));
    }

    #[test]
    fn parse_env_filters_pwd() {
        let output = "HOME=/home/user\0PWD=/tmp\0SHELL=/bin/bash\0";
        let env = parse_probed_env(output);
        assert_eq!(env.len(), 2);
        assert!(!env.contains_key("PWD"));
        assert_eq!(env.get("HOME"), Some(&"/home/user".to_string()));
    }

    #[test]
    fn from_str_roundtrip() {
        for variant in [
            UserEnvProbe::None,
            UserEnvProbe::LoginShell,
            UserEnvProbe::InteractiveShell,
            UserEnvProbe::LoginInteractiveShell,
        ] {
            assert_eq!(UserEnvProbe::from_str(variant.as_str()).unwrap(), variant);
        }
    }

    #[test]
    fn from_str_invalid() {
        assert!(UserEnvProbe::from_str("bogus").is_err());
    }

    #[test]
    fn default_is_login_interactive() {
        assert_eq!(UserEnvProbe::default(), UserEnvProbe::LoginInteractiveShell);
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
