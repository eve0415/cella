//! Shared shell detection, quoting, and command wrapping.
//!
//! Used by `exec`, `shell`, and `env_cache` to consistently detect the
//! container user's shell and wrap commands in a login shell.

use tracing::{debug, warn};

use cella_backend::{ContainerBackend, ExecOptions};

/// Detect the best available shell for a user inside a container.
///
/// Tries, in order:
/// 1. `$SHELL` environment variable
/// 2. `/etc/passwd` entry for the user
/// 3. Probing `/bin/zsh`, `/bin/bash`, `/bin/sh`
pub async fn detect_shell(
    client: &dyn ContainerBackend,
    container_id: &str,
    user: &str,
) -> String {
    if let Some(shell) = detect_shell_from_env(client, container_id, user).await {
        return shell;
    }
    if let Some(shell) = detect_shell_from_passwd(client, container_id, user).await {
        return shell;
    }
    if let Some(shell) = detect_shell_by_probing(client, container_id, user).await {
        return shell;
    }

    warn!("Could not detect shell, falling back to /bin/sh");
    "/bin/sh".to_string()
}

/// POSIX-safe shell quoting: wrap each argument in single quotes.
///
/// Single quotes inside arguments are escaped as `'\''` (end quote, escaped
/// literal quote, restart quote).
pub fn shell_quote(args: &[String]) -> String {
    args.iter()
        .map(|arg| {
            if arg.is_empty() {
                "''".to_string()
            } else {
                format!("'{}'", arg.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Wrap a command in a login shell with `exec` for proper signal propagation.
///
/// Returns `[shell, "-lc", "exec <quoted_command>"]`.
/// The `exec` replaces the shell process with the command so that signals
/// (e.g. SIGTERM from Docker) reach the actual process directly.
pub fn wrap_in_login_shell(shell: &str, command: &[String]) -> Vec<String> {
    let quoted = shell_quote(command);
    vec![
        shell.to_string(),
        "-lc".to_string(),
        format!("exec {quoted}"),
    ]
}

/// Try to detect the shell from the `$SHELL` environment variable.
async fn detect_shell_from_env(
    client: &dyn ContainerBackend,
    container_id: &str,
    user: &str,
) -> Option<String> {
    let result = client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: vec![
                    "sh".to_string(),
                    "-c".to_string(),
                    "echo $SHELL".to_string(),
                ],
                user: Some(user.to_string()),
                env: None,
                working_dir: None,
            },
        )
        .await
        .ok()?;

    let shell = result.stdout.trim().to_string();
    if !shell.is_empty() && shell != "$SHELL" {
        debug!("Detected shell from $SHELL: {shell}");
        return Some(shell);
    }
    None
}

/// Try to detect the shell from `/etc/passwd`.
async fn detect_shell_from_passwd(
    client: &dyn ContainerBackend,
    container_id: &str,
    user: &str,
) -> Option<String> {
    let result = client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: vec![
                    "sh".to_string(),
                    "-c".to_string(),
                    format!("getent passwd {user} 2>/dev/null || grep '^{user}:' /etc/passwd"),
                ],
                user: Some(user.to_string()),
                env: None,
                working_dir: None,
            },
        )
        .await
        .ok()?;

    let output = result.stdout.trim().to_string();
    let shell = output.split(':').nth(6)?.trim();
    if !shell.is_empty() {
        debug!("Detected shell from passwd: {shell}");
        return Some(shell.to_string());
    }
    None
}

/// Probe common shell paths to find one that exists.
async fn detect_shell_by_probing(
    client: &dyn ContainerBackend,
    container_id: &str,
    user: &str,
) -> Option<String> {
    for candidate in &["/bin/zsh", "/bin/bash", "/bin/sh"] {
        if let Ok(result) = client
            .exec_command(
                container_id,
                &ExecOptions {
                    cmd: vec![
                        "test".to_string(),
                        "-x".to_string(),
                        (*candidate).to_string(),
                    ],
                    user: Some(user.to_string()),
                    env: None,
                    working_dir: None,
                },
            )
            .await
            && result.exit_code == 0
        {
            debug!("Detected shell by probing: {candidate}");
            return Some((*candidate).to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quote_simple_args() {
        let args: Vec<String> = vec!["claude".into(), "--version".into()];
        assert_eq!(shell_quote(&args), "'claude' '--version'");
    }

    #[test]
    fn shell_quote_args_with_spaces() {
        let args: Vec<String> = vec!["echo".into(), "hello world".into()];
        assert_eq!(shell_quote(&args), "'echo' 'hello world'");
    }

    #[test]
    fn shell_quote_args_with_single_quotes() {
        let args: Vec<String> = vec!["echo".into(), "it's".into()];
        assert_eq!(shell_quote(&args), "'echo' 'it'\\''s'");
    }

    #[test]
    fn shell_quote_empty_string() {
        let args: Vec<String> = vec!["cmd".into(), String::new()];
        assert_eq!(shell_quote(&args), "'cmd' ''");
    }

    #[test]
    fn shell_quote_special_chars() {
        let args: Vec<String> = vec!["echo".into(), "$HOME && rm -rf /".into()];
        assert_eq!(shell_quote(&args), "'echo' '$HOME && rm -rf /'");
    }

    #[test]
    fn wrap_in_login_shell_basic() {
        let cmd: Vec<String> = vec!["claude".into(), "--dangerously-skip-permissions".into()];
        let wrapped = wrap_in_login_shell("/bin/zsh", &cmd);
        assert_eq!(
            wrapped,
            vec![
                "/bin/zsh",
                "-lc",
                "exec 'claude' '--dangerously-skip-permissions'"
            ]
        );
    }

    #[test]
    fn wrap_in_login_shell_with_spaces() {
        let cmd: Vec<String> = vec!["echo".into(), "hello world".into()];
        let wrapped = wrap_in_login_shell("/bin/bash", &cmd);
        assert_eq!(
            wrapped,
            vec!["/bin/bash", "-lc", "exec 'echo' 'hello world'"]
        );
    }

    #[test]
    fn shell_quote_no_args() {
        let args: Vec<String> = vec![];
        assert_eq!(shell_quote(&args), "");
    }

    #[test]
    fn shell_quote_single_arg() {
        let args: Vec<String> = vec!["ls".into()];
        assert_eq!(shell_quote(&args), "'ls'");
    }

    #[test]
    fn shell_quote_multiple_single_quotes() {
        let args: Vec<String> = vec!["it's o'clock".into()];
        assert_eq!(shell_quote(&args), "'it'\\''s o'\\''clock'");
    }

    #[test]
    fn shell_quote_newline_in_arg() {
        let args: Vec<String> = vec!["line1\nline2".into()];
        assert_eq!(shell_quote(&args), "'line1\nline2'");
    }

    #[test]
    fn shell_quote_dollar_sign() {
        let args: Vec<String> = vec!["$HOME".into()];
        assert_eq!(shell_quote(&args), "'$HOME'");
    }

    #[test]
    fn shell_quote_backslash() {
        let args: Vec<String> = vec!["path\\to\\file".into()];
        assert_eq!(shell_quote(&args), "'path\\to\\file'");
    }

    #[test]
    fn shell_quote_semicolons_and_pipes() {
        let args: Vec<String> = vec!["cmd1; cmd2 | cmd3".into()];
        assert_eq!(shell_quote(&args), "'cmd1; cmd2 | cmd3'");
    }

    #[test]
    fn wrap_in_login_shell_single_command() {
        let cmd: Vec<String> = vec!["ls".into()];
        let wrapped = wrap_in_login_shell("/bin/sh", &cmd);
        assert_eq!(wrapped, vec!["/bin/sh", "-lc", "exec 'ls'"]);
    }

    #[test]
    fn wrap_in_login_shell_with_single_quotes() {
        let cmd: Vec<String> = vec!["echo".into(), "it's".into()];
        let wrapped = wrap_in_login_shell("/bin/bash", &cmd);
        assert_eq!(wrapped, vec!["/bin/bash", "-lc", "exec 'echo' 'it'\\''s'"]);
    }

    #[test]
    fn wrap_in_login_shell_preserves_shell_path() {
        let cmd: Vec<String> = vec!["test".into()];
        let wrapped = wrap_in_login_shell("/usr/local/bin/zsh", &cmd);
        assert_eq!(wrapped[0], "/usr/local/bin/zsh");
    }

    #[test]
    fn wrap_in_login_shell_always_uses_lc_flag() {
        let cmd: Vec<String> = vec!["anything".into()];
        let wrapped = wrap_in_login_shell("/bin/sh", &cmd);
        assert_eq!(wrapped[1], "-lc");
    }

    #[test]
    fn wrap_in_login_shell_exec_prefix() {
        let cmd: Vec<String> = vec!["sleep".into(), "10".into()];
        let wrapped = wrap_in_login_shell("/bin/sh", &cmd);
        assert!(wrapped[2].starts_with("exec "));
    }

    #[test]
    fn wrap_in_login_shell_empty_command() {
        let cmd: Vec<String> = vec![];
        let wrapped = wrap_in_login_shell("/bin/sh", &cmd);
        assert_eq!(wrapped.len(), 3);
        assert_eq!(wrapped[2], "exec ");
    }

    #[test]
    fn shell_quote_tab_in_arg() {
        let args: Vec<String> = vec!["col1\tcol2".into()];
        assert_eq!(shell_quote(&args), "'col1\tcol2'");
    }

    #[test]
    fn shell_quote_unicode_arg() {
        let args: Vec<String> = vec!["echo".into(), "\u{1f600}".into()];
        assert_eq!(shell_quote(&args), "'echo' '\u{1f600}'");
    }

    #[test]
    fn wrap_in_login_shell_many_args() {
        let cmd: Vec<String> = vec!["cmd".into(), "arg1".into(), "arg2".into(), "arg3".into()];
        let wrapped = wrap_in_login_shell("/bin/bash", &cmd);
        assert_eq!(wrapped[2], "exec 'cmd' 'arg1' 'arg2' 'arg3'");
    }

    #[test]
    fn shell_quote_all_single_quotes() {
        let args: Vec<String> = vec!["'''".into()];
        assert_eq!(shell_quote(&args), "''\\'''\\'''\\'''");
    }
}
