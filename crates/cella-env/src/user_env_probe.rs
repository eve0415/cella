//! userEnvProbe spec implementation.
//!
//! Generates the command to probe the container user's environment
//! and parses the output, mirroring the official devcontainer CLI's
//! marker-wrapped, multi-method approach.

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "cli", derive(clap::ValueEnum))]
#[cfg_attr(feature = "cli", clap(rename_all = "camelCase"))]
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
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "none" => Ok(Self::None),
            "loginShell" => Ok(Self::LoginShell),
            "interactiveShell" => Ok(Self::InteractiveShell),
            "loginInteractiveShell" => Ok(Self::LoginInteractiveShell),
            _ => Err(()),
        }
    }
}

/// A strategy for reading the probed environment, in priority order.
///
/// `cat /proc/self/environ` (the kernel's NUL-delimited environ file) is tried
/// first because it needs no external tool; `printenv` (newline-delimited) is
/// the fallback for shells/images without it. Mirrors the official CLI.
#[derive(Clone, Copy, Debug)]
pub struct ProbeMethod {
    pub command: &'static str,
    pub separator: char,
}

pub const PROBE_METHODS: &[ProbeMethod] = &[
    ProbeMethod {
        command: "cat /proc/self/environ",
        separator: '\0',
    },
    ProbeMethod {
        command: "printenv",
        separator: '\n',
    },
];

/// Build the argv to probe the user's environment with `method`.
///
/// Wraps the inner command in `marker` so shell-startup noise can be stripped
/// from the output. Returns `None` when `probe_type` is `None`.
///
/// PowerShell (`pwsh` / `pwsh-preview`) uses `-Login -Command` or `-Command`
/// instead of POSIX flags. Because PowerShell's `echo` is an alias for
/// `Write-Output` (which always appends a newline, treating `-n` as literal
/// data), the marker write uses `[Console]::Write(...)` instead. Similarly,
/// `/proc/self/environ` is not reliable in PowerShell, so the PowerShell path
/// uses `Get-ChildItem Env:` with `printenv`-style newline separation and
/// ignores `method` for the env-dump command.
pub fn probe_command(
    probe_type: UserEnvProbe,
    shell: &str,
    marker: &str,
    method: ProbeMethod,
) -> Option<Vec<String>> {
    if probe_type == UserEnvProbe::None {
        return None;
    }
    let name = std::path::Path::new(shell)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    if name == "pwsh" || name == "pwsh-preview" {
        // PowerShell: use [Console]::Write for no-newline marker and
        // Get-ChildItem Env: for newline-delimited KEY=VALUE output.
        let wrapped = format!(
            "[Console]::Write('{marker}'); \
             Get-ChildItem Env: | ForEach-Object {{ \"$($_.Name)=$($_.Value)\" }}; \
             [Console]::Write('{marker}')"
        );
        let mut args = vec![shell.to_string()];
        if matches!(
            probe_type,
            UserEnvProbe::LoginShell | UserEnvProbe::LoginInteractiveShell
        ) {
            args.push("-Login".to_string());
        }
        args.push("-Command".to_string());
        args.push(wrapped);
        Some(args)
    } else {
        let wrapped = format!("echo -n {marker}; {}; echo -n {marker}", method.command);
        let flags = probe_type.shell_flags()?;
        Some(vec![
            shell.to_string(),
            flags.to_string(),
            "-c".to_string(),
            wrapped,
        ])
    }
}

/// Parse probe output into a KEY=VALUE map.
///
/// Extracts the text between the first and last `marker` (stripping any
/// shell-startup noise outside them), then splits on `separator`. Returns an
/// empty map when the markers are absent so the caller can fall back to another
/// probe method. Drops `PWD`.
pub fn parse_probed_env(output: &str, marker: &str, separator: char) -> HashMap<String, String> {
    let Some(start) = output.find(marker) else {
        return HashMap::new();
    };
    let inner_start = start + marker.len();
    let Some(end) = output.rfind(marker) else {
        return HashMap::new();
    };
    if end < inner_start {
        return HashMap::new();
    }
    output[inner_start..end]
        .split(separator)
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

    // ── probe_command ────────────────────────────────────────────────────────

    #[test]
    fn probe_command_none_returns_none() {
        assert!(probe_command(UserEnvProbe::None, "/bin/bash", "M", PROBE_METHODS[0]).is_none());
    }

    #[test]
    fn probe_command_login_shell_wraps_with_marker() {
        let marker = "abc-123";
        let cmd = probe_command(
            UserEnvProbe::LoginShell,
            "/bin/bash",
            marker,
            PROBE_METHODS[0],
        )
        .unwrap();
        assert_eq!(cmd[0], "/bin/bash");
        assert_eq!(cmd[1], "-l");
        assert_eq!(cmd[2], "-c");
        assert!(
            cmd[3].contains(&format!("echo -n {marker}")),
            "inner command must be marker-wrapped: {cmd:?}"
        );
        assert!(
            cmd[3].contains("cat /proc/self/environ"),
            "inner command must use /proc/self/environ: {cmd:?}"
        );
    }

    #[test]
    fn probe_command_interactive_shell() {
        let cmd = probe_command(
            UserEnvProbe::InteractiveShell,
            "/bin/zsh",
            "M",
            PROBE_METHODS[0],
        )
        .unwrap();
        assert_eq!(cmd[0], "/bin/zsh");
        assert_eq!(cmd[1], "-i");
        assert_eq!(cmd[2], "-c");
    }

    #[test]
    fn probe_command_login_interactive_shell() {
        let cmd = probe_command(
            UserEnvProbe::LoginInteractiveShell,
            "/bin/bash",
            "M",
            PROBE_METHODS[0],
        )
        .unwrap();
        assert_eq!(cmd[1], "-li");
    }

    #[test]
    fn probe_command_pwsh_login_interactive_uses_login_flag() {
        let marker = "M";
        let cmd = probe_command(
            UserEnvProbe::LoginInteractiveShell,
            "/usr/bin/pwsh",
            marker,
            PROBE_METHODS[0],
        )
        .unwrap();
        assert_eq!(cmd[0], "/usr/bin/pwsh");
        assert_eq!(cmd[1], "-Login");
        assert_eq!(cmd[2], "-Command");
        // Must use [Console]::Write for no-newline marker (not echo -n)
        assert!(
            cmd[3].contains("[Console]::Write('M')"),
            "pwsh marker must use [Console]::Write, not echo -n: {cmd:?}"
        );
        // Must use Get-ChildItem Env: instead of cat /proc/self/environ
        assert!(
            cmd[3].contains("Get-ChildItem Env:"),
            "pwsh env dump must use Get-ChildItem Env:: {cmd:?}"
        );
        assert!(
            !cmd[3].contains("echo -n"),
            "pwsh must NOT use echo -n (emits -n as literal data): {cmd:?}"
        );
    }

    #[test]
    fn probe_command_pwsh_interactive_no_login_flag() {
        let marker = "M";
        let cmd = probe_command(
            UserEnvProbe::InteractiveShell,
            "/usr/bin/pwsh",
            marker,
            PROBE_METHODS[0],
        )
        .unwrap();
        assert_eq!(cmd[0], "/usr/bin/pwsh");
        assert_eq!(cmd[1], "-Command");
        assert!(
            !cmd[2].contains("echo -n"),
            "pwsh must NOT use echo -n: {cmd:?}"
        );
    }

    #[test]
    fn probe_command_pwsh_preview_login() {
        let cmd = probe_command(
            UserEnvProbe::LoginShell,
            "/usr/bin/pwsh-preview",
            "M",
            PROBE_METHODS[0],
        )
        .unwrap();
        assert_eq!(cmd[1], "-Login");
        assert_eq!(cmd[2], "-Command");
    }

    /// Regression: PowerShell `echo -n` outputs `-n` literally (emits a
    /// newline before the probed content), corrupting the first env var.
    /// The fix is `[Console]::Write(marker)` which writes without a newline.
    #[test]
    fn probe_command_pwsh_marker_no_newline_regression() {
        let marker = "UNIQUE-MARKER-123";
        let cmd = probe_command(
            UserEnvProbe::LoginInteractiveShell,
            "/usr/bin/pwsh",
            marker,
            PROBE_METHODS[0],
        )
        .unwrap();
        let inner = &cmd[cmd.len() - 1];
        assert!(
            inner.contains(&format!("[Console]::Write('{marker}')")),
            "marker must be written with [Console]::Write to avoid leading newline: {inner}"
        );
        assert!(
            !inner.contains("echo"),
            "must not use echo for marker in pwsh — echo is Write-Output and adds a newline: {inner}"
        );
    }

    /// Regression: PowerShell does not reliably pass NUL bytes through stdout,
    /// so `cat /proc/self/environ` output is unreliable under pwsh. The probe
    /// must use `Get-ChildItem Env:` (newline-delimited) instead, regardless
    /// of which `ProbeMethod` is passed.
    #[test]
    fn probe_command_pwsh_uses_get_child_item_not_proc_environ() {
        for method in PROBE_METHODS {
            let cmd = probe_command(
                UserEnvProbe::LoginInteractiveShell,
                "/usr/bin/pwsh",
                "M",
                *method,
            )
            .unwrap();
            let inner = &cmd[cmd.len() - 1];
            assert!(
                inner.contains("Get-ChildItem Env:"),
                "pwsh probe must use Get-ChildItem Env: (method={}): {inner}",
                method.command
            );
            assert!(
                !inner.contains("/proc/self/environ"),
                "pwsh probe must NOT use /proc/self/environ (NUL passthrough unreliable): {inner}"
            );
        }
    }

    #[test]
    fn probe_command_printenv_fallback_method() {
        let marker = "abc-123";
        let cmd = probe_command(
            UserEnvProbe::LoginShell,
            "/bin/bash",
            marker,
            PROBE_METHODS[1],
        )
        .unwrap();
        assert!(
            cmd[3].contains("printenv"),
            "second method must use printenv: {cmd:?}"
        );
    }

    // ── parse_probed_env ─────────────────────────────────────────────────────

    #[test]
    fn parse_nul_delimited_clean() {
        let marker = "abc-123";
        let output =
            format!("{marker}HOME=/home/user\0PATH=/usr/bin:/bin\0SHELL=/bin/bash\0{marker}");
        let env = parse_probed_env(&output, marker, '\0');
        assert_eq!(env.get("HOME"), Some(&"/home/user".to_string()));
        assert_eq!(env.get("PATH"), Some(&"/usr/bin:/bin".to_string()));
        assert_eq!(env.get("SHELL"), Some(&"/bin/bash".to_string()));
        assert_eq!(env.len(), 3);
    }

    #[test]
    fn parse_strips_leading_startup_noise() {
        let marker = "abc-123";
        let output = format!("Welcome to my shell!\n{marker}HOME=/home/u\0PATH=/bin\0{marker}");
        let env = parse_probed_env(&output, marker, '\0');
        assert_eq!(env.get("HOME"), Some(&"/home/u".to_string()));
        assert_eq!(env.get("PATH"), Some(&"/bin".to_string()));
        assert_eq!(env.len(), 2, "startup noise must not appear as env vars");
    }

    #[test]
    fn parse_newline_delimited_printenv_form() {
        let marker = "abc-123";
        let output = format!("{marker}HOME=/home/user\nPATH=/usr/bin\nSHELL=/bin/bash\n{marker}");
        let env = parse_probed_env(&output, marker, '\n');
        assert_eq!(env.get("HOME"), Some(&"/home/user".to_string()));
        assert_eq!(env.get("PATH"), Some(&"/usr/bin".to_string()));
        assert_eq!(env.get("SHELL"), Some(&"/bin/bash".to_string()));
        assert_eq!(env.len(), 3);
    }

    #[test]
    fn parse_no_marker_returns_empty() {
        let env = parse_probed_env("HOME=/home/user\0PATH=/usr/bin\0", "abc-123", '\0');
        assert!(
            env.is_empty(),
            "absent marker must yield empty map (caller falls back)"
        );
    }

    #[test]
    fn parse_filters_pwd() {
        let marker = "abc-123";
        let output = format!("{marker}HOME=/home/user\0PWD=/tmp\0SHELL=/bin/bash\0{marker}");
        let env = parse_probed_env(&output, marker, '\0');
        assert!(!env.contains_key("PWD"), "PWD must be dropped");
        assert_eq!(env.len(), 2);
    }

    #[test]
    fn parse_empty_inner() {
        let marker = "abc-123";
        let output = format!("{marker}{marker}");
        let env = parse_probed_env(&output, marker, '\0');
        assert!(env.is_empty());
    }

    #[test]
    fn parse_entry_without_equals_skipped() {
        let marker = "M";
        let output = format!("{marker}GOOD=value\0BADENTRY\0ALSO_GOOD=val2\0{marker}");
        let env = parse_probed_env(&output, marker, '\0');
        assert_eq!(env.len(), 2);
        assert!(env.contains_key("GOOD"));
        assert!(env.contains_key("ALSO_GOOD"));
    }

    // ── merge_env ────────────────────────────────────────────────────────────

    #[test]
    fn merge_env_later_entry_wins_config_over_cli() {
        let probed = HashMap::from([("PATH".to_string(), "/usr/bin".to_string())]);
        let entries = vec![
            "FOO=cli".to_string(),
            "FOO=cfg".to_string(),
            "BAR=cli".to_string(),
        ];
        let merged = merge_env(&probed, &entries);
        assert!(merged.contains(&"FOO=cfg".to_string()));
        assert!(!merged.contains(&"FOO=cli".to_string()));
        assert!(merged.contains(&"BAR=cli".to_string()));
        assert!(merged.contains(&"PATH=/usr/bin".to_string()));
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

    // ── UserEnvProbe enum ────────────────────────────────────────────────────

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
}
