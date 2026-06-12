use std::path::PathBuf;
use std::time::{Duration, Instant};

use clap::{Args, ValueEnum};
use serde_json::json;
use tracing::debug;

use cella_backend::{BackendEndpoint, ExecOptions};

use super::OutputFormat;
use super::up::{UpArgs, UpContext, UpRenderData, UpResult, output_result};

/// Editor to open for the dev container.
#[derive(Clone, ValueEnum)]
pub enum EditorChoice {
    /// VS Code (stable).
    Code,
    /// VS Code Insiders.
    Insiders,
    /// Cursor.
    Cursor,
}

use crate::picker;

#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum CodeError {
    #[error("`{name}` not found in PATH")]
    #[diagnostic(code(cella::code::editor_not_in_path), help("{help_text}"))]
    EditorNotInPath { name: String, help_text: String },

    #[error("editor binary not found: {path}")]
    #[diagnostic(code(cella::code::editor_binary_not_found))]
    EditorBinaryNotFound { path: String },

    #[error("failed to launch `{name}`: {reason}")]
    #[diagnostic(
        code(cella::code::editor_launch_failed),
        help("Ensure `{name}` is properly installed and can be launched from the terminal.")
    )]
    EditorLaunchFailed { name: String, reason: String },

    #[error(
        "DOCKER_HOST points to a remote {protocol} host, but `cella code` requires a local Docker host"
    )]
    #[diagnostic(
        code(cella::code::remote_docker_host),
        help(
            "For remote containers:\n  1. SSH into the remote host\n  2. Run `cella code` there, or\n  3. Use VS Code Remote-SSH + forward Docker socket"
        )
    )]
    RemoteDockerHost { protocol: String },

    #[error("`cella code` requires the Docker backend (VS Code attach is Docker-specific)")]
    #[diagnostic(
        code(cella::code::non_docker_backend),
        help(
            "VS Code's Dev Containers extension attaches through the Docker API and cannot see containers managed by the apple/container backend.\nWorkaround: enable VS Code's experimental `dev.containers.experimentalAppleContainerSupport` setting and let the extension manage the container itself."
        )
    )]
    NonDockerBackend,
}

/// Open VS Code connected to the dev container.
///
/// Ensures the container is running (auto-up if needed), runs `postAttachCommand`,
/// then opens VS Code using the `attached-container` remote URI scheme.
#[derive(Args)]
pub struct CodeArgs {
    #[command(flatten)]
    pub up: UpArgs,

    /// Editor to open (defaults to code).
    #[arg(long, value_enum, default_value = "code", conflicts_with = "binary")]
    pub editor: EditorChoice,

    /// Custom editor binary name or path.
    #[arg(long)]
    pub binary: Option<String>,

    /// Target a specific compose service (defaults to primary service).
    #[arg(long)]
    pub service: Option<String>,
}

impl CodeArgs {
    pub const fn is_text_output(&self) -> bool {
        self.up.is_text_output()
    }

    pub async fn execute(self, progress: crate::progress::Progress) -> miette::Result<()> {
        // 1. Check for remote Docker (unsupported)
        check_local_docker()?;

        // 2. Resolve editor binary (fail fast before container work)
        let editor = resolve_editor_binary(&self.editor, self.binary.as_deref())?;
        debug!("Resolved editor binary: {}", editor.display());

        // 3. Ensure container is up
        let build_no_cache = self.up.build.build_no_cache;
        let strict = self.up.strict.clone();
        let output_format = self.up.output.resolve();
        let mut up = self.up;
        picker::resolve_up_workspace(&mut up).await;
        let ctx = UpContext::new(&up, progress)
            .await
            .map_err(super::boxed_err_to_report)?;
        let _title_guard = crate::title::push_for_workspace(
            ctx.client.as_ref(),
            &ctx.resolved.workspace_root,
            &ctx.container_nm,
            self.service.as_deref(),
            None,
            "code",
        )
        .await;

        // Reject non-Docker backends — VS Code attach URI is Docker-specific
        if ctx.client.kind() != cella_backend::BackendKind::Docker {
            return Err(CodeError::NonDockerBackend.into());
        }
        let result = if ctx.is_compose() {
            super::compose_up::compose_ensure_up(&ctx)
                .await
                .map_err(super::boxed_err_to_report)?
        } else {
            ctx.ensure_up(build_no_cache, &strict)
                .await
                .map_err(super::boxed_err_to_report)?
        };

        // 4. Resolve the attach target (id + `/`-trimmed name)
        let (container_id, container_name) =
            resolve_attach_target(&ctx, &result.container_id, self.service.as_deref()).await?;

        // 5. Build the attached-container URI
        let endpoint = ctx.client.endpoint();
        let uri = build_vscode_uri(&container_name, endpoint.as_ref(), &result.workspace_folder);
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
                let name = editor
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("editor")
                    .to_string();
                return Err(CodeError::EditorLaunchFailed {
                    name,
                    reason: e.to_string(),
                }
                .into());
            }
        }

        // 7. Poll for VS Code Server connection
        let remote_user = &result.remote_user;
        let connected = poll_vscode_server(
            ctx.client.as_ref(),
            &container_id,
            remote_user,
            &ctx.progress,
        )
        .await;

        if connected {
            ctx.progress.hint("VS Code connected to dev container.");
        }

        // 8. Output result
        emit_code_result(&output_format, &result, &container_id, &uri);

        Ok(())
    }
}

/// Resolve the container the editor should attach to, returning its `(id, name)`.
///
/// The returned name is `/`-trimmed (as carried by [`cella_backend::ContainerInfo`]).
/// Inspects `container_id` — revalidating it right before the editor launches —
/// and, for `--service`, resolves the requested compose service container from it.
async fn resolve_attach_target(
    ctx: &UpContext,
    container_id: &str,
    service: Option<&str>,
) -> miette::Result<(String, String)> {
    let container = ctx.client.inspect_container(container_id).await?;
    let resolved = super::resolve_service_container(ctx.client.as_ref(), container, service)
        .await
        .map_err(super::boxed_err_to_report)?;
    Ok((resolved.id, resolved.name))
}

/// Emit the `cella code` result.
///
/// `code` has no official devcontainer-CLI analogue, so its JSON shape keeps
/// the granular `outcome` (running/started/created) plus a `uri` key. The
/// text path reuses the shared `up` renderer.
fn emit_code_result(
    output_format: &OutputFormat,
    result: &UpResult,
    container_id: &str,
    uri: &str,
) {
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
        OutputFormat::Auto | OutputFormat::Text => {
            output_result(&UpRenderData {
                format: &OutputFormat::Text,
                outcome: "success",
                state: &result.outcome,
                container_id,
                remote_user: &result.remote_user,
                workspace_folder: &result.workspace_folder,
                ssh_agent_proxy: result.ssh_agent_proxy.as_ref(),
                compose_project_name: result.compose_project_name.as_deref(),
                configuration: result.configuration.as_ref(),
                merged_configuration: result.merged_configuration.as_ref(),
            });
        }
    }
}

/// Build the VS Code attached-container remote URI.
///
/// Format: `vscode-remote://attached-container+{hex}{workspace_folder}`, where
/// `hex` is the byte-hex of a JSON payload. The Dev Containers extension
/// hex-decodes the authority; when the decoded bytes start with `{` it parses
/// them as JSON and reads `containerName` (the `/`-prefixed name matching
/// `docker inspect` `.Name`) plus `settings.host`/`settings.context` to pick
/// its Docker endpoint (`DOCKER_HOST`/`DOCKER_CONTEXT`, context wins). The
/// `settings` key is omitted when the backend exposes no endpoint, leaving
/// the extension on its own defaults.
///
/// `container_name` is the `/`-trimmed name from [`cella_backend::ContainerInfo`];
/// the leading `/` is re-prepended here.
fn build_vscode_uri(
    container_name: &str,
    endpoint: Option<&BackendEndpoint>,
    workspace_folder: &str,
) -> String {
    let mut payload = json!({ "containerName": format!("/{container_name}") });
    if let Some(endpoint) = endpoint {
        payload["settings"] = match endpoint {
            BackendEndpoint::HostUri(host) => json!({ "host": host }),
            BackendEndpoint::NamedContext(context) => json!({ "context": context }),
        };
    }
    let hex = hex::encode(payload.to_string());
    format!("vscode-remote://attached-container+{hex}{workspace_folder}")
}

/// Resolve which editor binary to use.
fn resolve_editor_binary(
    editor: &EditorChoice,
    binary: Option<&str>,
) -> Result<PathBuf, CodeError> {
    let name = if let Some(b) = binary {
        if b.contains('/') {
            let path = PathBuf::from(b);
            if path.exists() {
                return Ok(path);
            }
            return Err(CodeError::EditorBinaryNotFound {
                path: b.to_string(),
            });
        }
        b.to_string()
    } else {
        match editor {
            EditorChoice::Code => "code".to_string(),
            EditorChoice::Insiders => "code-insiders".to_string(),
            EditorChoice::Cursor => "cursor".to_string(),
        }
    };

    which_binary(&name)
}

/// Look up a binary name in PATH.
fn which_binary(name: &str) -> Result<PathBuf, CodeError> {
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    Err(CodeError::EditorNotInPath {
        name: name.to_string(),
        help_text: editor_not_found_help(name),
    })
}

/// Platform-specific help text for the `EditorNotInPath` diagnostic.
fn editor_not_found_help(name: &str) -> String {
    match name {
        "code" | "code-insiders" => {
            if cfg!(target_os = "macos") {
                format!(
                    "Open VS Code \u{2192} Cmd+Shift+P \u{2192} \
                     \"Shell Command: Install '{name}' command in PATH\""
                )
            } else {
                format!("Install the `{name}` package or add it to your PATH")
            }
        }
        "cursor" => "Install Cursor from https://cursor.com and add it to your PATH".to_string(),
        _ => format!("Ensure `{name}` is installed and available in your PATH"),
    }
}

/// Check that Docker is local (not a remote host via SSH or TCP).
fn check_local_docker() -> Result<(), CodeError> {
    let docker_host = std::env::var("DOCKER_HOST").ok();
    if let Some(ref host) = docker_host {
        if host.starts_with("ssh://") {
            return Err(CodeError::RemoteDockerHost {
                protocol: "SSH".to_string(),
            });
        }
        if host.starts_with("tcp://") && !is_localhost_tcp(host) {
            return Err(CodeError::RemoteDockerHost {
                protocol: "TCP".to_string(),
            });
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

/// Build the shell probe command that detects any VS Code-family server.
///
/// Returns a three-element `["sh", "-c", <script>]` command that expands
/// `$HOME` for the exec user and succeeds (exit 0) as soon as any of
/// `.vscode-server`, `.vscode-server-insiders`, or `.cursor-server` has a
/// `bin/` subdirectory. This covers stable VS Code, VS Code Insiders, and
/// Cursor, and works for any home directory (root, custom, etc.).
fn vscode_server_probe_cmd() -> Vec<String> {
    let script = "for d in .vscode-server .vscode-server-insiders .cursor-server; \
         do [ -d \"$HOME/$d/bin\" ] && exit 0; done; exit 1";
    vec!["sh".to_string(), "-c".to_string(), script.to_string()]
}

/// Poll for VS Code Server installation inside the container.
///
/// Checks `$HOME/{.vscode-server,.vscode-server-insiders,.cursor-server}/bin`
/// for the exec user every 2 seconds, up to 60 seconds. Covers stable VS Code,
/// VS Code Insiders, and Cursor, and any home directory (including root).
/// Returns `true` if a server was detected, `false` on timeout.
async fn poll_vscode_server(
    client: &dyn cella_backend::ContainerBackend,
    container_id: &str,
    remote_user: &str,
    progress: &crate::progress::Progress,
) -> bool {
    let step = progress.step("Waiting for VS Code to connect...");
    let start = Instant::now();
    let timeout = Duration::from_mins(1);
    let interval = Duration::from_secs(2);

    loop {
        let check_result = client
            .exec_command(
                container_id,
                &ExecOptions {
                    cmd: vscode_server_probe_cmd(),
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

    /// Decode the hex authority of an attached-container URI back to its JSON.
    ///
    /// Strips the scheme prefix and the trailing workspace folder, then
    /// hex-decodes the remaining authority payload to a UTF-8 string.
    fn decode_uri_payload(uri: &str, workspace_folder: &str) -> String {
        let authority = uri
            .strip_prefix("vscode-remote://attached-container+")
            .expect("URI should carry the attached-container prefix");
        let hex = authority
            .strip_suffix(workspace_folder)
            .expect("URI should end with the workspace folder");
        let bytes = hex::decode(hex).expect("valid hex payload");
        String::from_utf8(bytes).expect("payload should be valid UTF-8")
    }

    #[test]
    fn build_uri_basic() {
        let uri = build_vscode_uri("my-container", None, "/workspaces/myapp");
        assert!(uri.starts_with("vscode-remote://attached-container+"));
        assert!(uri.ends_with("/workspaces/myapp"));
        let payload = decode_uri_payload(&uri, "/workspaces/myapp");
        assert_eq!(payload, r#"{"containerName":"/my-container"}"#);
    }

    #[test]
    fn build_uri_with_endpoint_pins_exact_payload() {
        let endpoint = BackendEndpoint::HostUri("unix:///var/run/docker.sock".to_string());
        let uri = build_vscode_uri("deadbeef", Some(&endpoint), "/workspaces/test");
        let payload = decode_uri_payload(&uri, "/workspaces/test");
        assert_eq!(
            payload,
            r#"{"containerName":"/deadbeef","settings":{"host":"unix:///var/run/docker.sock"}}"#
        );
    }

    #[test]
    fn build_uri_roundtrip_docker_host() {
        let endpoint =
            BackendEndpoint::HostUri("unix:///Users/x/.colima/default/docker.sock".to_string());
        let uri = build_vscode_uri("my-container", Some(&endpoint), "/workspaces/app");
        let payload = decode_uri_payload(&uri, "/workspaces/app");
        let value: serde_json::Value = serde_json::from_str(&payload).expect("valid JSON");
        assert_eq!(value["containerName"], "/my-container");
        assert_eq!(
            value["settings"]["host"],
            "unix:///Users/x/.colima/default/docker.sock"
        );
        assert!(value["settings"].get("context").is_none());
    }

    #[test]
    fn build_uri_roundtrip_docker_context() {
        let endpoint = BackendEndpoint::NamedContext("orbstack".to_string());
        let uri = build_vscode_uri("my-container", Some(&endpoint), "/workspaces/app");
        let payload = decode_uri_payload(&uri, "/workspaces/app");
        let value: serde_json::Value = serde_json::from_str(&payload).expect("valid JSON");
        assert_eq!(value["containerName"], "/my-container");
        assert_eq!(value["settings"]["context"], "orbstack");
        assert!(value["settings"].get("host").is_none());
    }

    #[test]
    fn build_uri_no_endpoint_omits_settings() {
        let uri = build_vscode_uri("my-container", None, "/workspaces/app");
        let payload = decode_uri_payload(&uri, "/workspaces/app");
        let value: serde_json::Value = serde_json::from_str(&payload).expect("valid JSON");
        assert_eq!(value["containerName"], "/my-container");
        assert!(
            value.get("settings").is_none(),
            "settings key must be absent without an endpoint"
        );
    }

    #[test]
    fn build_uri_shape_decodes_to_json_object() {
        let endpoint = BackendEndpoint::HostUri("tcp://localhost:2375".to_string());
        let uri = build_vscode_uri("c", Some(&endpoint), "/workspaces/x");
        assert!(uri.starts_with("vscode-remote://attached-container+"));
        assert!(uri.ends_with("/workspaces/x"));
        let payload = decode_uri_payload(&uri, "/workspaces/x");
        assert!(payload.starts_with('{'), "payload must be a JSON object");
    }

    #[test]
    fn build_uri_prepends_leading_slash_exactly_once() {
        // Name input never carries a leading slash; output always begins `"/`.
        let uri = build_vscode_uri("my-container", None, "");
        let payload = decode_uri_payload(&uri, "");
        let value: serde_json::Value = serde_json::from_str(&payload).expect("valid JSON");
        let name = value["containerName"].as_str().expect("string name");
        assert_eq!(name, "/my-container");
        assert!(!name.starts_with("//"), "no double-slash prefix");
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
    fn code_error_editor_not_in_path_has_help() {
        use miette::Diagnostic;
        let err = CodeError::EditorNotInPath {
            name: "code".into(),
            help_text: "Install code".into(),
        };
        let help = err.help().expect("should have help text");
        assert!(help.to_string().contains("Install code"));
    }

    #[test]
    fn code_error_editor_launch_failed_has_help() {
        use miette::Diagnostic;
        let err = CodeError::EditorLaunchFailed {
            name: "code".into(),
            reason: "not found".into(),
        };
        let help = err.help().expect("should have help text");
        assert!(help.to_string().contains("code"));
        assert!(help.to_string().contains("terminal"));
    }

    #[test]
    fn code_error_remote_docker_has_help() {
        use miette::Diagnostic;
        let err = CodeError::RemoteDockerHost {
            protocol: "TCP".into(),
        };
        assert!(
            err.to_string().contains("TCP"),
            "protocol should appear in error body"
        );
        let help = err.help().expect("should have help text");
        assert!(help.to_string().contains("Remote-SSH"));
    }

    #[test]
    fn build_uri_empty_workspace() {
        let uri = build_vscode_uri("abc", None, "");
        let payload = decode_uri_payload(&uri, "");
        assert_eq!(payload, r#"{"containerName":"/abc"}"#);
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
        let result = resolve_editor_binary(&EditorChoice::Code, Some("/nonexistent/editor"));
        assert!(result.is_err());
    }

    #[test]
    fn resolve_editor_binary_name_not_in_path() {
        let result =
            resolve_editor_binary(&EditorChoice::Code, Some("totally-nonexistent-editor-xyz"));
        assert!(result.is_err());
    }

    // ── check_local_docker ─────────────────────────────────────────
    // Note: check_local_docker reads DOCKER_HOST env var. Setting env vars
    // in tests is unsafe in Edition 2024 and racy across threads, so we
    // test the underlying is_localhost_tcp helper instead for most logic.

    // ── is_localhost_tcp additional cases ───────────────────────────

    #[test]
    fn is_localhost_tcp_127_no_port() {
        assert!(is_localhost_tcp("tcp://127.0.0.1"));
    }

    #[test]
    fn is_localhost_tcp_ipv6_loopback_raw() {
        assert!(is_localhost_tcp("tcp://::1"));
    }

    #[test]
    fn is_localhost_tcp_empty_authority() {
        assert!(!is_localhost_tcp("tcp://"));
    }

    // ── build_vscode_uri ───────────────────────────────────────────

    #[test]
    fn build_uri_with_nested_workspace() {
        let uri = build_vscode_uri("abc", None, "/workspaces/project/sub/dir");
        assert!(uri.starts_with("vscode-remote://attached-container+"));
        assert!(uri.ends_with("/workspaces/project/sub/dir"));
    }

    #[test]
    fn code_error_non_docker_has_help() {
        use miette::Diagnostic;
        let err = CodeError::NonDockerBackend;
        let help = err.help().expect("should have help text");
        assert!(
            help.to_string()
                .contains("dev.containers.experimentalAppleContainerSupport")
        );
    }

    #[test]
    fn code_error_remote_docker_renders_multiline_help() {
        let report: miette::Report = CodeError::RemoteDockerHost {
            protocol: "SSH".into(),
        }
        .into();
        let mut rendered = String::new();
        miette::GraphicalReportHandler::new()
            .render_report(&mut rendered, report.as_ref())
            .unwrap();
        assert!(rendered.contains("help:"), "missing help section");
        assert!(rendered.contains("1."), "missing numbered list");
        assert!(rendered.contains("Remote-SSH"), "missing VS Code hint");
    }

    #[test]
    fn code_error_renders_help_on_separate_line() {
        let report: miette::Report = CodeError::EditorNotInPath {
            name: "code".into(),
            help_text: "add it to PATH".into(),
        }
        .into();
        let mut rendered = String::new();
        miette::GraphicalReportHandler::new()
            .render_report(&mut rendered, report.as_ref())
            .unwrap();
        assert!(rendered.contains("help:"), "missing help section");
        assert!(!rendered.contains("\\n"), "literal \\n in output");
    }

    // ── vscode_server_probe_cmd ────────────────────────────────────

    #[test]
    fn probe_cmd_has_three_elements() {
        let cmd = vscode_server_probe_cmd();
        assert_eq!(cmd.len(), 3, "must be [sh, -c, <script>]");
        assert_eq!(cmd[0], "sh");
        assert_eq!(cmd[1], "-c");
    }

    #[test]
    fn probe_cmd_script_covers_all_server_dirs() {
        let script = &vscode_server_probe_cmd()[2];
        assert!(
            script.contains(".vscode-server"),
            "must include .vscode-server"
        );
        assert!(
            script.contains(".vscode-server-insiders"),
            "must include .vscode-server-insiders"
        );
        assert!(
            script.contains(".cursor-server"),
            "must include .cursor-server"
        );
    }

    #[test]
    fn probe_cmd_script_uses_home_variable_not_hardcoded_path() {
        let script = &vscode_server_probe_cmd()[2];
        assert!(
            script.contains("$HOME"),
            "must use $HOME, not a hardcoded path"
        );
        assert!(
            !script.contains("/home/"),
            "must not hardcode /home/ — use $HOME instead"
        );
    }
}
