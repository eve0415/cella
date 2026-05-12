//! Shared shell detection, quoting, and command wrapping.

use tracing::{debug, warn};

use cella_backend::{ContainerBackend, ExecOptions};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellSource {
    CliFlag,
    Preferred,
    Detected,
    Fallback,
}

#[derive(Debug, Clone)]
pub struct ShellResolution {
    pub shell: String,
    pub source: ShellSource,
}

/// Resolve the shell to use, respecting user preferences.
///
/// Priority: preference list (probed) -> existing detection -> /bin/sh.
pub async fn resolve_shell(
    client: &dyn ContainerBackend,
    container_id: &str,
    user: &str,
    preferred: &[String],
) -> ShellResolution {
    if !preferred.is_empty()
        && let Some(shell) = probe_preferred(client, container_id, user, preferred).await
    {
        return ShellResolution {
            shell,
            source: ShellSource::Preferred,
        };
    }

    let shell = detect_shell(client, container_id, user).await;
    let source = if shell == "/bin/sh" {
        ShellSource::Fallback
    } else {
        ShellSource::Detected
    };
    ShellResolution { shell, source }
}

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

async fn probe_preferred(
    client: &dyn ContainerBackend,
    container_id: &str,
    user: &str,
    preferred: &[String],
) -> Option<String> {
    for candidate in preferred {
        let resolved = if candidate.contains('/') {
            probe_full_path(client, container_id, user, candidate).await
        } else {
            probe_short_name(client, container_id, user, candidate).await
        };
        if let Some(shell) = resolved {
            debug!("Resolved preferred shell {candidate} -> {shell}");
            return Some(shell);
        }
        debug!("Preferred shell {candidate} not available");
    }
    None
}

fn is_valid_shell_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || b == b'_'
                || b == b'.'
                || b == b'/'
                || b == b'-'
                || b == b'+'
        })
}

async fn probe_short_name(
    client: &dyn ContainerBackend,
    container_id: &str,
    user: &str,
    name: &str,
) -> Option<String> {
    if !is_valid_shell_name(name) {
        warn!("Ignoring invalid shell name in preference: {name:?}");
        return None;
    }

    let quoted = shell_quote(&[name.to_string()]);
    let result = client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: vec![
                    "sh".to_string(),
                    "-c".to_string(),
                    format!("command -v {quoted}"),
                ],
                user: Some(user.to_string()),
                env: None,
                working_dir: None,
            },
        )
        .await
        .ok()?;

    if result.exit_code == 0 {
        let path = result.stdout.trim().to_string();
        if !path.is_empty() {
            return Some(path);
        }
    }

    for prefix in &["/bin/", "/usr/bin/", "/usr/local/bin/"] {
        let path = format!("{prefix}{name}");
        if probe_full_path(client, container_id, user, &path)
            .await
            .is_some()
        {
            return Some(path);
        }
    }
    None
}

async fn probe_full_path(
    client: &dyn ContainerBackend,
    container_id: &str,
    user: &str,
    path: &str,
) -> Option<String> {
    if !is_valid_shell_name(path) {
        warn!("Ignoring invalid shell path in preference: {path:?}");
        return None;
    }

    let result = client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: vec!["test".to_string(), "-x".to_string(), path.to_string()],
                user: Some(user.to_string()),
                env: None,
                working_dir: None,
            },
        )
        .await
        .ok()?;

    if result.exit_code == 0 {
        Some(path.to_string())
    } else {
        None
    }
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

    #[test]
    fn shell_resolution_types() {
        let res = ShellResolution {
            shell: "/bin/zsh".to_string(),
            source: ShellSource::Preferred,
        };
        assert_eq!(res.source, ShellSource::Preferred);
    }

    #[test]
    fn valid_shell_names_accepted() {
        assert!(is_valid_shell_name("zsh"));
        assert!(is_valid_shell_name("bash"));
        assert!(is_valid_shell_name("/bin/zsh"));
        assert!(is_valid_shell_name("/usr/local/bin/fish"));
        assert!(is_valid_shell_name("my-shell_v2.0"));
        assert!(is_valid_shell_name("shell+extra"));
    }

    #[test]
    fn injection_shell_names_rejected() {
        assert!(!is_valid_shell_name(""));
        assert!(!is_valid_shell_name("zsh; rm -rf /"));
        assert!(!is_valid_shell_name("$(evil)"));
        assert!(!is_valid_shell_name("shell`whoami`"));
        assert!(!is_valid_shell_name("sh && echo pwned"));
        assert!(!is_valid_shell_name("zsh\nmalicious"));
    }

    // ── Mock backend for async resolve_shell tests ─────────────────

    use std::collections::VecDeque;
    use std::path::Path;
    use std::sync::Mutex;

    use cella_backend::{BackendCapabilities, BackendError, BackendKind, BoxFuture, ExecResult};

    struct MockBackend {
        responses: Mutex<VecDeque<ExecResult>>,
    }

    impl MockBackend {
        fn from_responses(responses: Vec<ExecResult>) -> Self {
            Self {
                responses: Mutex::new(VecDeque::from(responses)),
            }
        }
    }

    fn ok_result(stdout: &str) -> ExecResult {
        ExecResult {
            exit_code: 0,
            stdout: stdout.to_string(),
            stderr: String::new(),
        }
    }

    fn fail_result() -> ExecResult {
        ExecResult {
            exit_code: 1,
            stdout: String::new(),
            stderr: String::new(),
        }
    }

    macro_rules! panic_method {
        ($name:ident, $($arg:ident: $ty:ty),* => $ret:ty) => {
            fn $name<'a>(&'a self, $(_: $ty),*) -> BoxFuture<'a, $ret> {
                panic!(concat!(stringify!($name), " should not be called"));
            }
        };
    }

    impl ContainerBackend for MockBackend {
        fn kind(&self) -> BackendKind {
            BackendKind::Docker
        }

        fn capabilities(&self) -> BackendCapabilities {
            BackendCapabilities {
                compose: false,
                managed_agent: false,
            }
        }

        fn exec_command<'a>(
            &'a self,
            _container_id: &'a str,
            _opts: &'a ExecOptions,
        ) -> BoxFuture<'a, Result<ExecResult, BackendError>> {
            let result = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("MockBackend: no exec_command response configured");
            Box::pin(async move { Ok(result) })
        }

        panic_method!(find_container, w: &'a Path => Result<Option<cella_backend::ContainerInfo>, BackendError>);
        panic_method!(create_container, o: &'a cella_backend::CreateContainerOptions => Result<String, BackendError>);
        panic_method!(start_container, id: &'a str => Result<(), BackendError>);
        panic_method!(stop_container, id: &'a str => Result<(), BackendError>);
        fn remove_container<'a>(
            &'a self,
            _id: &'a str,
            _remove_volumes: bool,
        ) -> BoxFuture<'a, Result<(), BackendError>> {
            panic!("remove_container should not be called");
        }
        panic_method!(inspect_container, id: &'a str => Result<cella_backend::ContainerInfo, BackendError>);
        fn list_cella_containers(
            &self,
            _running_only: bool,
        ) -> BoxFuture<'_, Result<Vec<cella_backend::ContainerInfo>, BackendError>> {
            panic!("list_cella_containers should not be called");
        }
        fn find_compose_service<'a>(
            &'a self,
            _project: &'a str,
            _service: &'a str,
        ) -> BoxFuture<'a, Result<Option<cella_backend::ContainerInfo>, BackendError>> {
            panic!("find_compose_service should not be called");
        }
        fn find_container_by_label<'a>(
            &'a self,
            _label: &'a str,
        ) -> BoxFuture<'a, Result<Option<cella_backend::ContainerInfo>, BackendError>> {
            panic!("find_container_by_label should not be called");
        }
        fn container_logs<'a>(
            &'a self,
            _id: &'a str,
            _tail: u32,
        ) -> BoxFuture<'a, Result<String, BackendError>> {
            panic!("container_logs should not be called");
        }
        fn exec_stream<'a>(
            &'a self,
            _container_id: &'a str,
            _opts: &'a ExecOptions,
            _stdout: Box<dyn std::io::Write + Send + 'a>,
            _stderr: Box<dyn std::io::Write + Send + 'a>,
        ) -> BoxFuture<'a, Result<ExecResult, BackendError>> {
            panic!("exec_stream should not be called");
        }
        fn exec_interactive<'a>(
            &'a self,
            _container_id: &'a str,
            _opts: &'a cella_backend::InteractiveExecOptions,
        ) -> BoxFuture<'a, Result<i64, BackendError>> {
            panic!("exec_interactive should not be called");
        }
        fn exec_detached<'a>(
            &'a self,
            _container_id: &'a str,
            _opts: &'a ExecOptions,
        ) -> BoxFuture<'a, Result<String, BackendError>> {
            panic!("exec_detached should not be called");
        }
        panic_method!(pull_image, image: &'a str => Result<(), BackendError>);
        panic_method!(build_image, opts: &'a cella_backend::BuildOptions => Result<String, BackendError>);
        panic_method!(image_exists, image: &'a str => Result<bool, BackendError>);
        panic_method!(inspect_image_details, image: &'a str => Result<cella_backend::ImageDetails, BackendError>);
        fn upload_files<'a>(
            &'a self,
            _container_id: &'a str,
            _files: &'a [cella_backend::FileToUpload],
        ) -> BoxFuture<'a, Result<(), BackendError>> {
            panic!("upload_files should not be called");
        }
        fn ping(&self) -> BoxFuture<'_, Result<(), BackendError>> {
            panic!("ping should not be called");
        }
        fn host_gateway(&self) -> &'static str {
            "host.docker.internal"
        }
        fn detect_platform(&self) -> BoxFuture<'_, Result<cella_backend::Platform, BackendError>> {
            panic!("detect_platform should not be called");
        }
        fn detect_container_arch(&self) -> BoxFuture<'_, Result<String, BackendError>> {
            panic!("detect_container_arch should not be called");
        }
        fn inspect_image_env<'a>(
            &'a self,
            _image: &'a str,
        ) -> BoxFuture<'a, Result<Vec<String>, BackendError>> {
            panic!("inspect_image_env should not be called");
        }
        fn inspect_image_user<'a>(
            &'a self,
            _image: &'a str,
        ) -> BoxFuture<'a, Result<String, BackendError>> {
            panic!("inspect_image_user should not be called");
        }
        fn ensure_network(&self) -> BoxFuture<'_, Result<(), BackendError>> {
            panic!("ensure_network should not be called");
        }
        fn ensure_container_network<'a>(
            &'a self,
            _container_id: &'a str,
            _repo_path: &'a Path,
        ) -> BoxFuture<'a, Result<(), BackendError>> {
            panic!("ensure_container_network should not be called");
        }
        fn get_container_ip<'a>(
            &'a self,
            _container_id: &'a str,
        ) -> BoxFuture<'a, Result<Option<String>, BackendError>> {
            panic!("get_container_ip should not be called");
        }
        fn ensure_agent_provisioned<'a>(
            &'a self,
            _version: &'a str,
            _arch: &'a str,
            _skip_checksum: bool,
        ) -> BoxFuture<'a, Result<(), BackendError>> {
            panic!("ensure_agent_provisioned should not be called");
        }
        fn write_agent_addr<'a>(
            &'a self,
            _container_id: &'a str,
            _addr: &'a str,
            _token: &'a str,
        ) -> BoxFuture<'a, Result<(), BackendError>> {
            panic!("write_agent_addr should not be called");
        }
        fn agent_volume_mount(&self) -> (String, String, bool) {
            panic!("agent_volume_mount should not be called");
        }
        fn prune_old_agent_versions<'a>(
            &'a self,
            _current_version: &'a str,
        ) -> BoxFuture<'a, Result<(), BackendError>> {
            panic!("prune_old_agent_versions should not be called");
        }
    }

    // ── Async resolve_shell tests ──────────────────────────────────

    #[tokio::test]
    async fn resolve_picks_first_available_preferred() {
        let mock = MockBackend::from_responses(vec![
            ok_result("/usr/bin/fish"), // command -v fish
        ]);
        let res = resolve_shell(&mock, "ctr", "user", &["fish".to_string()]).await;
        assert_eq!(res.shell, "/usr/bin/fish");
        assert_eq!(res.source, ShellSource::Preferred);
    }

    #[tokio::test]
    async fn resolve_skips_unavailable_tries_next() {
        let mock = MockBackend::from_responses(vec![
            fail_result(),          // command -v fish
            fail_result(),          // test -x /bin/fish
            fail_result(),          // test -x /usr/bin/fish
            fail_result(),          // test -x /usr/local/bin/fish
            ok_result("/bin/bash"), // command -v bash
        ]);
        let res = resolve_shell(
            &mock,
            "ctr",
            "user",
            &["fish".to_string(), "bash".to_string()],
        )
        .await;
        assert_eq!(res.shell, "/bin/bash");
        assert_eq!(res.source, ShellSource::Preferred);
    }

    #[tokio::test]
    async fn resolve_falls_through_when_no_preferred_available() {
        let mock = MockBackend::from_responses(vec![
            fail_result(),          // command -v nonexistent
            fail_result(),          // test -x /bin/nonexistent
            fail_result(),          // test -x /usr/bin/nonexistent
            fail_result(),          // test -x /usr/local/bin/nonexistent
            ok_result("/bin/bash"), // detect_shell: echo $SHELL
        ]);
        let res = resolve_shell(&mock, "ctr", "user", &["nonexistent".to_string()]).await;
        assert_eq!(res.shell, "/bin/bash");
        assert_eq!(res.source, ShellSource::Detected);
    }

    #[tokio::test]
    async fn resolve_empty_preferred_uses_detection() {
        let mock = MockBackend::from_responses(vec![
            ok_result("/bin/zsh"), // detect_shell: echo $SHELL
        ]);
        let res = resolve_shell(&mock, "ctr", "user", &[]).await;
        assert_eq!(res.shell, "/bin/zsh");
        assert_eq!(res.source, ShellSource::Detected);
    }

    #[tokio::test]
    async fn resolve_full_path_probed_directly() {
        let mock = MockBackend::from_responses(vec![
            ok_result(""), // test -x /home/linuxbrew/.linuxbrew/bin/fish
        ]);
        let res = resolve_shell(
            &mock,
            "ctr",
            "user",
            &["/home/linuxbrew/.linuxbrew/bin/fish".to_string()],
        )
        .await;
        assert_eq!(res.shell, "/home/linuxbrew/.linuxbrew/bin/fish");
        assert_eq!(res.source, ShellSource::Preferred);
    }

    #[tokio::test]
    async fn resolve_fallback_when_detection_returns_bin_sh() {
        let mock = MockBackend::from_responses(vec![
            ok_result("/bin/sh"), // detect_shell: echo $SHELL
        ]);
        let res = resolve_shell(&mock, "ctr", "user", &[]).await;
        assert_eq!(res.shell, "/bin/sh");
        assert_eq!(res.source, ShellSource::Fallback);
    }
}
