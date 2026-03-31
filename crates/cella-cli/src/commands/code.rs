use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use clap::Args;
use serde_json::json;
use tracing::debug;

use cella_docker::ExecOptions;

use super::up::{OutputFormat, UpArgs, UpContext, output_result};

use crate::picker;

/// Open VS Code connected to the dev container.
///
/// Ensures the container is running (auto-up if needed), runs `postAttachCommand`,
/// then opens VS Code using the `attached-container` remote URI scheme.
#[derive(Args)]
#[allow(clippy::struct_excessive_bools)]
pub struct CodeArgs {
    #[command(flatten)]
    pub up: UpArgs,

    /// Open VS Code Insiders instead of stable.
    #[arg(long, conflicts_with_all = ["cursor", "binary"])]
    pub insider: bool,

    /// Open Cursor instead of VS Code.
    #[arg(long, conflicts_with_all = ["insider", "binary"])]
    pub cursor: bool,

    /// Custom editor binary name or path.
    #[arg(long, conflicts_with_all = ["insider", "cursor"])]
    pub binary: Option<String>,

    /// Target a specific compose service (defaults to primary service).
    #[arg(long)]
    pub service: Option<String>,
}

impl CodeArgs {
    pub const fn is_text_output(&self) -> bool {
        self.up.is_text_output()
    }

    pub async fn execute(
        self,
        progress: crate::progress::Progress,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // 1. Check for remote Docker (unsupported)
        check_local_docker()?;

        // 2. Resolve editor binary (fail fast before container work)
        let editor = resolve_editor_binary(self.insider, self.cursor, self.binary.as_deref())?;
        debug!("Resolved editor binary: {}", editor.display());

        // 3. Ensure container is up
        let build_no_cache = self.up.build_no_cache;
        let strict = self.up.strict.clone();
        let output_format = self.up.output.clone();
        let mut up = self.up;
        picker::resolve_up_workspace(&mut up).await;
        let ctx = UpContext::new(&up, progress).await?;
        let result = ctx.ensure_up(build_no_cache, &strict).await?;

        // 4. Resolve compose service if needed
        let container_id = if self.service.is_some() {
            let container = ctx.client.inspect_container(&result.container_id).await?;
            let resolved =
                super::resolve_service_container(&ctx.client, container, self.service.as_deref())
                    .await?;
            resolved.id
        } else {
            result.container_id.clone()
        };

        // 5. Build the attached-container URI
        let uri = build_vscode_uri(&container_id, &result.workspace_folder);
        debug!("VS Code URI: {uri}");

        // 6. Launch editor
        let step = ctx.progress.step("Opening editor...");
        let spawn_result = std::process::Command::new(&editor)
            .arg("--folder-uri")
            .arg(&uri)
            .spawn();

        match spawn_result {
            Ok(_child) => step.finish(),
            Err(e) => {
                step.fail("failed to launch");
                return Err(format!(
                    "Failed to launch `{}`: {e}\n\n{}",
                    editor.display(),
                    editor_install_hint(&editor)
                )
                .into());
            }
        }

        // 7. Poll for VS Code Server connection
        let remote_user = &result.remote_user;
        let connected =
            poll_vscode_server(&ctx.client, &container_id, remote_user, &ctx.progress).await;

        if connected {
            ctx.progress.hint("VS Code connected to dev container.");
        }

        // 8. Output result
        match output_format {
            OutputFormat::Json => {
                let output = json!({
                    "outcome": result.outcome,
                    "containerId": container_id,
                    "remoteUser": result.remote_user,
                    "remoteWorkspaceFolder": result.workspace_folder,
                    "uri": uri,
                });
                println!(
                    "{}",
                    serde_json::to_string_pretty(&output).unwrap_or_default()
                );
            }
            OutputFormat::Text => {
                output_result(
                    &OutputFormat::Text,
                    &result.outcome,
                    &container_id,
                    &result.remote_user,
                    &result.workspace_folder,
                );
            }
        }

        Ok(())
    }
}

/// Hex-encode a container ID for the attached-container URI scheme.
///
/// Each ASCII byte of the container ID is converted to its two-character hex
/// representation. For example, `"a"` (0x61) becomes `"61"`.
fn hex_encode_container_id(id: &str) -> String {
    use std::fmt::Write;
    let mut encoded = String::with_capacity(id.len() * 2);
    for byte in id.bytes() {
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
}

/// Build the VS Code attached-container remote URI.
///
/// Format: `vscode-remote://attached-container+{hex_encoded_id}{workspace_path}`
fn build_vscode_uri(container_id: &str, workspace_folder: &str) -> String {
    let hex_id = hex_encode_container_id(container_id);
    format!("vscode-remote://attached-container+{hex_id}{workspace_folder}")
}

/// Resolve which editor binary to use.
///
/// Returns the path to the editor binary. If the binary is not found, returns
/// an error with platform-specific installation instructions.
fn resolve_editor_binary(
    insider: bool,
    cursor: bool,
    binary: Option<&str>,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let name = if let Some(b) = binary {
        // If it contains a path separator, treat as path
        if b.contains('/') {
            let path = PathBuf::from(b);
            if path.exists() {
                return Ok(path);
            }
            return Err(format!("Editor binary not found: {b}").into());
        }
        b.to_string()
    } else if insider {
        "code-insiders".to_string()
    } else if cursor {
        "cursor".to_string()
    } else {
        "code".to_string()
    };

    which_binary(&name)
}

/// Look up a binary name in PATH.
fn which_binary(name: &str) -> Result<PathBuf, Box<dyn std::error::Error>> {
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    Err(format!(
        "`{name}` not found in PATH.\n\n{}",
        editor_not_found_help(name)
    )
    .into())
}

/// Platform-specific help text when an editor binary is not found.
fn editor_not_found_help(name: &str) -> String {
    match name {
        "code" | "code-insiders" => {
            if cfg!(target_os = "macos") {
                format!(
                    "To fix:\n  \
                     Open VS Code \u{2192} Cmd+Shift+P \u{2192} \
                     \"Shell Command: Install '{name}' command in PATH\""
                )
            } else {
                format!("To fix:\n  Install the `{name}` package or add it to your PATH")
            }
        }
        "cursor" => {
            "To fix:\n  Install Cursor from https://cursor.com and add it to your PATH".to_string()
        }
        _ => format!("Ensure `{name}` is installed and available in your PATH"),
    }
}

/// Help text when editor launch fails (binary exists but spawn fails).
fn editor_install_hint(editor: &Path) -> String {
    let name = editor
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("editor");
    format!("Ensure `{name}` is properly installed and can be launched from the terminal.")
}

/// Check that Docker is local (not a remote host via SSH or TCP).
fn check_local_docker() -> Result<(), Box<dyn std::error::Error>> {
    let docker_host = std::env::var("DOCKER_HOST").ok();
    if let Some(ref host) = docker_host {
        if host.starts_with("ssh://") {
            return Err("`cella code` requires a local Docker host.\n\n\
                 DOCKER_HOST is set to an SSH remote. For remote containers:\n  \
                 1. SSH into the remote host\n  \
                 2. Run `cella code` there, or\n  \
                 3. Use VS Code Remote-SSH + forward Docker socket"
                .into());
        }
        if host.starts_with("tcp://") && !is_localhost_tcp(host) {
            return Err("`cella code` requires a local Docker host.\n\n\
                 DOCKER_HOST points to a remote TCP host. For remote containers:\n  \
                 1. SSH into the remote host\n  \
                 2. Run `cella code` there, or\n  \
                 3. Use VS Code Remote-SSH + forward Docker socket"
                .into());
        }
    }
    Ok(())
}

/// Check if a `tcp://` Docker host URL points to localhost.
fn is_localhost_tcp(url: &str) -> bool {
    let authority = url.strip_prefix("tcp://").unwrap_or(url);
    // Strip bracket notation and port for comparison
    let normalized = authority
        .trim_start_matches('[')
        .replace("]:", ":")
        .replace(']', "");
    // Check if the host part (before port) is a localhost address
    let host = normalized.split(':').next().unwrap_or("");
    // For IPv6 "::1" without brackets, check the full authority (minus port if obvious)
    matches!(host, "localhost" | "127.0.0.1")
        || authority.starts_with("[::1]")
        || authority == "::1"
}

/// Poll for VS Code Server installation inside the container.
///
/// Checks for `~/.vscode-server/bin` directory every 2 seconds, up to 60 seconds.
/// Returns `true` if server was detected, `false` on timeout.
async fn poll_vscode_server(
    client: &cella_docker::DockerClient,
    container_id: &str,
    remote_user: &str,
    progress: &crate::progress::Progress,
) -> bool {
    let step = progress.step("Waiting for VS Code to connect...");
    let start = Instant::now();
    let timeout = Duration::from_secs(60);
    let interval = Duration::from_secs(2);

    loop {
        let check_result = client
            .exec_command(
                container_id,
                &ExecOptions {
                    cmd: vec![
                        "test".to_string(),
                        "-d".to_string(),
                        format!("/home/{remote_user}/.vscode-server/bin"),
                    ],
                    user: Some(remote_user.to_string()),
                    env: None,
                    working_dir: None,
                },
            )
            .await;

        if let Ok(result) = check_result
            && result.exit_code == 0
        {
            let elapsed = start.elapsed().as_secs_f32();
            step.finish();
            debug!("VS Code Server detected after {elapsed:.1}s");
            return true;
        }

        if start.elapsed() > timeout {
            step.fail("timed out");
            progress.warn(
                "VS Code did not connect within 60s (it may still connect in the background).",
            );
            return false;
        }

        tokio::time::sleep(interval).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_encode_empty_string() {
        assert_eq!(hex_encode_container_id(""), "");
    }

    #[test]
    fn hex_encode_single_char() {
        // 'a' = 0x61
        assert_eq!(hex_encode_container_id("a"), "61");
    }

    #[test]
    fn hex_encode_known_container_id() {
        // A short example: "abc123" -> each char's hex
        // a=61, b=62, c=63, 1=31, 2=32, 3=33
        assert_eq!(hex_encode_container_id("abc123"), "616263313233");
    }

    #[test]
    fn hex_encode_hex_chars() {
        // All hex digits: 0-9 a-f
        let input = "0123456789abcdef";
        let expected = "30313233343536373839616263646566";
        assert_eq!(hex_encode_container_id(input), expected);
    }

    #[test]
    fn hex_encode_full_docker_id() {
        // 64-char container ID should produce 128-char encoded string
        let id = "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2";
        let encoded = hex_encode_container_id(id);
        assert_eq!(encoded.len(), 128);
    }

    #[test]
    fn build_uri_basic() {
        let uri = build_vscode_uri("abc123", "/workspaces/myapp");
        assert_eq!(
            uri,
            "vscode-remote://attached-container+616263313233/workspaces/myapp"
        );
    }

    #[test]
    fn build_uri_with_full_id() {
        let id = "deadbeef";
        let uri = build_vscode_uri(id, "/workspaces/test");
        // d=64, e=65, a=61, d=64, b=62, e=65, e=65, f=66
        assert_eq!(
            uri,
            "vscode-remote://attached-container+6465616462656566/workspaces/test"
        );
    }

    #[test]
    fn is_localhost_tcp_variants() {
        assert!(is_localhost_tcp("tcp://localhost:2375"));
        assert!(is_localhost_tcp("tcp://127.0.0.1:2375"));
        assert!(is_localhost_tcp("tcp://[::1]:2375"));
        assert!(is_localhost_tcp("tcp://::1"));
        assert!(!is_localhost_tcp("tcp://192.168.1.100:2375"));
        assert!(!is_localhost_tcp("tcp://my-server:2375"));
    }

    #[test]
    fn editor_not_found_help_code() {
        let help = editor_not_found_help("code");
        assert!(help.contains("PATH"));
    }

    #[test]
    fn editor_not_found_help_cursor() {
        let help = editor_not_found_help("cursor");
        assert!(help.contains("cursor.com"));
    }

    #[test]
    fn editor_not_found_help_custom() {
        let help = editor_not_found_help("my-editor");
        assert!(help.contains("my-editor"));
    }

    #[test]
    fn editor_not_found_help_code_insiders() {
        let help = editor_not_found_help("code-insiders");
        assert!(help.contains("PATH"));
    }

    #[test]
    fn editor_install_hint_with_name() {
        let path = PathBuf::from("/usr/local/bin/code");
        let hint = editor_install_hint(&path);
        assert!(hint.contains("code"));
    }

    #[test]
    fn editor_install_hint_no_file_name() {
        let path = PathBuf::from("/");
        let hint = editor_install_hint(&path);
        assert!(hint.contains("editor"));
    }

    #[test]
    fn build_uri_empty_workspace() {
        let uri = build_vscode_uri("abc", "");
        assert_eq!(uri, "vscode-remote://attached-container+616263");
    }

    #[test]
    fn is_localhost_tcp_localhost_no_port() {
        // No port variant
        assert!(!is_localhost_tcp("tcp://remote:2375"));
    }

    #[test]
    fn is_localhost_tcp_ipv6_bracket() {
        assert!(is_localhost_tcp("tcp://[::1]:2376"));
    }

    #[test]
    fn resolve_editor_binary_missing_custom() {
        // A binary that doesn't exist should error
        let result = resolve_editor_binary(false, false, Some("/nonexistent/editor"));
        assert!(result.is_err());
    }

    #[test]
    fn resolve_editor_binary_name_not_in_path() {
        let result = resolve_editor_binary(false, false, Some("totally-nonexistent-editor-xyz"));
        assert!(result.is_err());
    }
}
