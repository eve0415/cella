//! Shared shell detection, quoting, and command wrapping.

use tracing::{debug, warn};

use cella_backend::{ContainerBackend, ExecOptions};

/// Detect the best available shell for a user inside a container.
pub async fn detect_shell(client: &dyn ContainerBackend, container_id: &str, user: &str) -> String {
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
pub fn wrap_in_login_shell(shell: &str, command: &[String]) -> Vec<String> {
    let quoted = shell_quote(command);
    vec![
        shell.to_string(),
        "-lc".to_string(),
        format!("exec {quoted}"),
    ]
}

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

/// Escape regex metacharacters so the string is treated as a literal in `grep -E`.
fn escape_regex(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for c in s.chars() {
        if matches!(
            c,
            '\\' | '.' | '[' | ']' | '(' | ')' | '{' | '}' | '*' | '+' | '?' | '^' | '$' | '|'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

async fn detect_shell_from_passwd(
    client: &dyn ContainerBackend,
    container_id: &str,
    user: &str,
) -> Option<String> {
    let escaped_shell = shell_quote(&[user.to_string()]);
    let escaped_regex = escape_regex(user);
    let result = client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: vec![
                    "sh".to_string(),
                    "-c".to_string(),
                    format!(
                        "getent passwd {escaped_shell} 2>/dev/null || grep '^{escaped_regex}:' /etc/passwd"
                    ),
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
    fn escape_regex_plain_username() {
        assert_eq!(escape_regex("vscode"), "vscode");
    }

    #[test]
    fn escape_regex_special_chars() {
        assert_eq!(escape_regex("foo.bar"), "foo\\.bar");
        assert_eq!(escape_regex("user$"), "user\\$");
        assert_eq!(escape_regex("a+b"), "a\\+b");
    }

    #[test]
    fn escape_regex_backslash() {
        assert_eq!(escape_regex("foo\\bar"), "foo\\\\bar");
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
}
