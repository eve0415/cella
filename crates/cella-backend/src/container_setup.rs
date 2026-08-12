//! Container setup helpers extracted from the CLI `up` command.
//!
//! These are pure business-logic functions that operate on a [`ContainerBackend`]
//! and devcontainer config values. They have no CLI or progress-reporting
//! dependencies.

use crate::{
    BackendError, ContainerBackend, ContainerState, ExecOptions, ExecResult, FileToUpload,
};
use tracing::{debug, info, warn};

// ── Host commands (initializeCommand) ─────────────────────────────────────

/// Run an `initializeCommand` (or similar host-side lifecycle command).
///
/// Supports string, array, and object (named) forms per the devcontainer spec.
///
/// # Errors
///
/// Returns an error if any individual command exits with a non-zero status.
pub fn run_host_command(
    phase: &str,
    value: &serde_json::Value,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    info!("Running {phase} on host");

    match value {
        serde_json::Value::String(_) | serde_json::Value::Array(_) => {
            run_host_command_value(phase, value)?;
        }
        // Object form runs the named commands in PARALLEL (official `Promise.all`
        // over `cliHost.ptyExec`). All entries run to completion; the first
        // error in iteration order is surfaced.
        serde_json::Value::Object(map) => {
            run_host_object_parallel(phase, map)?;
        }
        _ => {}
    }

    Ok(())
}

/// Run a single host lifecycle command value: a string via `sh -c`, an array as
/// argv (no shell). Other JSON types are a no-op.
fn run_host_command_value(
    phase: &str,
    value: &serde_json::Value,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    match value {
        serde_json::Value::String(s) => run_single_host_command(phase, &["sh", "-c", s]),
        serde_json::Value::Array(arr) => run_json_array_command(phase, arr),
        _ => Ok(()),
    }
}

/// Run the named entries of an object-form host lifecycle command in parallel
/// (one OS thread each), waiting for all to finish, then returning the first
/// error in iteration order.
fn run_host_object_parallel(
    phase: &str,
    map: &serde_json::Map<String, serde_json::Value>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    std::thread::scope(|scope| {
        let handles: Vec<_> = map
            .iter()
            .map(|(name, v)| {
                scope.spawn(move || {
                    info!("{phase} [{name}]");
                    run_host_command_value(phase, v)
                })
            })
            .collect();

        let mut first_err = None;
        for handle in handles {
            let outcome = handle
                .join()
                .unwrap_or_else(|_| Err(format!("{phase} host command thread panicked").into()));
            if let Err(e) = outcome
                && first_err.is_none()
            {
                first_err = Some(e);
            }
        }
        first_err.map_or(Ok(()), Err)
    })
}

fn run_json_array_command(
    phase: &str,
    arr: &[serde_json::Value],
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let cmd: Vec<String> = arr
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();
    if !cmd.is_empty() {
        let refs: Vec<&str> = cmd.iter().map(String::as_str).collect();
        run_single_host_command(phase, &refs)?;
    }
    Ok(())
}

fn run_single_host_command(
    phase: &str,
    cmd: &[&str],
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if cmd.is_empty() {
        return Ok(());
    }

    let status = std::process::Command::new(cmd[0])
        .args(&cmd[1..])
        .status()?;

    if !status.success() {
        return Err(format!(
            "{phase} failed with exit code {}",
            status.code().unwrap_or(-1)
        )
        .into());
    }

    Ok(())
}

// ── Pure conversion helpers ───────────────────────────────────────────────

/// Convert a JSON `remoteEnv` object to a vec of `KEY=value` strings.
pub fn map_env_object(value: Option<&serde_json::Value>) -> Vec<String> {
    value
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or("")))
                .collect()
        })
        .unwrap_or_default()
}

/// Convert [`cella_env::FileUpload`] items to [`cella_backend::FileToUpload`].
pub fn convert_uploads(uploads: &[cella_env::FileUpload]) -> Vec<FileToUpload> {
    uploads
        .iter()
        .map(|f| FileToUpload {
            path: f.container_path.clone(),
            content: f.content.clone(),
            mode: f.mode,
        })
        .collect()
}

/// Resolve the remote user from config and image metadata.
///
/// Priority: `remoteUser` (config) > `containerUser` (config) > `remoteUser`
/// (image metadata) > `containerUser` (image metadata) > `fallback` (typically
/// Docker USER or `"root"`).
pub fn resolve_remote_user(
    config: &serde_json::Value,
    image_meta_user: Option<&cella_features::ImageMetadataUserInfo>,
    fallback: &str,
) -> String {
    config
        .get("remoteUser")
        .and_then(|v| v.as_str())
        .or_else(|| config.get("containerUser").and_then(|v| v.as_str()))
        .or_else(|| image_meta_user.and_then(|m| m.remote_user.as_deref()))
        .or_else(|| image_meta_user.and_then(|m| m.container_user.as_deref()))
        .unwrap_or(fallback)
        .to_string()
}

// ── Container verification ────────────────────────────────────────────────

/// Verify that a container is in the `Running` state. Returns a backend
/// error (with log tail) if it has already exited.
///
/// # Errors
///
/// Returns an error if the container is not running.
pub async fn verify_container_running(
    client: &dyn ContainerBackend,
    container_id: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let info = client.inspect_container(container_id).await?;
    if info.state != ContainerState::Running {
        let logs = client.container_logs(container_id, 20).await?;
        return Err(BackendError::ContainerExitedImmediately {
            exit_code: info.exit_code.unwrap_or(-1),
            logs_tail: logs,
        }
        .into());
    }
    Ok(())
}

// ── In-container operation helpers ────────────────────────────────────────

/// Create a directory inside the container with the given mode (as root).
///
/// # Errors
///
/// Returns an error if the exec fails.
pub async fn mkdir_in_container(
    client: &dyn ContainerBackend,
    container_id: &str,
    dir: &str,
    mode: u32,
) -> Result<ExecResult, BackendError> {
    client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: vec![
                    "sh".to_string(),
                    "-c".to_string(),
                    format!("mkdir -p {dir} && chmod {mode:o} {dir}"),
                ],
                user: Some("root".to_string()),
                env: None,
                working_dir: None,
            },
        )
        .await
}

/// Recursively chown a directory inside the container.
pub async fn chown_in_container(
    client: &dyn ContainerBackend,
    container_id: &str,
    remote_user: &str,
    dir: &str,
) {
    let _ = client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: vec![
                    "chown".to_string(),
                    "-R".to_string(),
                    format!("{remote_user}:{remote_user}"),
                    dir.to_string(),
                ],
                user: Some("root".to_string()),
                env: None,
                working_dir: None,
            },
        )
        .await;
}

/// Create a directory, upload files, and fix ownership.
///
/// Returns `true` on success, `false` on any step failure.
pub async fn upload_to_container(
    client: &dyn ContainerBackend,
    container_id: &str,
    remote_user: &str,
    dir: &str,
    uploads: &[cella_env::FileUpload],
    context_label: &str,
) -> bool {
    if let Err(e) = mkdir_in_container(client, container_id, dir, 0o700).await {
        warn!("Failed to create {context_label} directory: {e}");
        return false;
    }

    let docker_files = convert_uploads(uploads);
    if let Err(e) = client.upload_files(container_id, &docker_files).await {
        warn!("Failed to upload {context_label} files: {e}");
    }

    chown_in_container(client, container_id, remote_user, dir).await;
    true
}

/// Check if a config already exists in the container (runs a test command).
pub async fn config_exists_in_container(
    client: &dyn ContainerBackend,
    container_id: &str,
    remote_user: &str,
    check_cmd: &[String],
) -> bool {
    client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: check_cmd.to_vec(),
                user: Some(remote_user.to_string()),
                env: None,
                working_dir: None,
            },
        )
        .await
        .is_ok_and(|r| r.exit_code == 0)
}

// ── SSH / Git setup ───────────────────────────────────────────────────────

/// Inject post-start environment forwarding into a running container.
///
/// Uploads files, runs user commands (e.g., gitignore merge), sets git
/// config, and runs privileged root commands. Never fails -- individual
/// steps log warnings and are skipped on error.
pub async fn inject_post_start(
    client: &dyn ContainerBackend,
    container_id: &str,
    post_start: &cella_env::PostStartInjection,
    remote_user: &str,
) {
    upload_ssh_files(client, container_id, &post_start.file_uploads, remote_user).await;

    for cmd in &post_start.user_commands {
        let result = client
            .exec_command(
                container_id,
                &ExecOptions {
                    cmd: cmd.clone(),
                    user: Some(remote_user.to_string()),
                    env: None,
                    working_dir: None,
                },
            )
            .await;
        match result {
            Ok(r) if r.exit_code != 0 => {
                warn!(
                    "Post-start command failed (exit {}): {}",
                    r.exit_code,
                    r.stderr.trim()
                );
            }
            Err(e) => {
                warn!("Failed to exec post-start command: {e}");
            }
            _ => {}
        }
    }

    apply_git_config(
        client,
        container_id,
        &post_start.git_config_commands,
        remote_user,
    )
    .await;

    // Execute privileged commands (e.g., CA trust store updates) as root.
    for cmd in &post_start.root_commands {
        let result = client
            .exec_command(
                container_id,
                &ExecOptions {
                    cmd: cmd.clone(),
                    user: Some("root".to_string()),
                    env: None,
                    working_dir: None,
                },
            )
            .await;
        match result {
            Ok(r) if r.exit_code != 0 => {
                warn!(
                    "Root command failed (exit {}): {}",
                    r.exit_code,
                    r.stderr.trim()
                );
            }
            Err(e) => {
                warn!("Failed to exec root command: {e}");
            }
            _ => {}
        }
    }
}

/// Upload SSH config files to the container's `.ssh` directory.
async fn upload_ssh_files(
    client: &dyn ContainerBackend,
    container_id: &str,
    uploads: &[cella_env::FileUpload],
    remote_user: &str,
) {
    if uploads.is_empty() {
        return;
    }

    let ssh_dir = cella_env::ssh_config::remote_ssh_dir(remote_user);
    if let Err(e) = mkdir_in_container(client, container_id, &ssh_dir, 0o700).await {
        warn!("Failed to create .ssh directory: {e}");
        return;
    }

    let docker_files = convert_uploads(uploads);
    if let Err(e) = client.upload_files(container_id, &docker_files).await {
        warn!("Failed to upload SSH config files: {e}");
    } else {
        chown_in_container(client, container_id, remote_user, &ssh_dir).await;
    }
}

/// Apply git config commands inside the container.
async fn apply_git_config(
    client: &dyn ContainerBackend,
    container_id: &str,
    commands: &[Vec<String>],
    remote_user: &str,
) {
    for cmd in commands {
        let result = client
            .exec_command(
                container_id,
                &ExecOptions {
                    cmd: cmd.clone(),
                    user: Some(remote_user.to_string()),
                    env: None,
                    working_dir: None,
                },
            )
            .await;
        match result {
            Ok(r) if r.exit_code != 0 => {
                warn!(
                    "git config failed (exit {}): {}",
                    r.exit_code,
                    r.stderr.trim()
                );
                break;
            }
            Err(e) => {
                warn!("Failed to exec git config: {e}");
                break;
            }
            _ => {}
        }
    }
}

/// Add `/cella/bin` and `~/.local/bin` to PATH in the container's shell
/// profile.
///
/// `/cella/bin` makes the cella CLI (symlinked to the agent binary)
/// discoverable. `~/.local/bin` is the XDG-standard user-local binary
/// directory used by `curl | bash` installers (Claude Code, uv, rustup,
/// etc.) — without it, tools installed by `install_tools_and_probe_env`
/// may not appear on PATH in worktree containers.
///
/// Each block has its own idempotency guard so the function is safe to
/// re-run and also patches containers that were created before the
/// `~/.local/bin` block existed.
async fn inject_snippets(
    client: &dyn ContainerBackend,
    container_id: &str,
    home: &str,
    snippets: &[(&str, &str)],
    profiles: &[&str],
) {
    for (guard, snippet) in snippets {
        for profile in profiles {
            let path = format!("{home}/{profile}");
            let cmd = format!(
                "if [ -f '{path}' ] && ! grep -q '{guard}' '{path}'; then printf '%s\\n' '{escaped}' >> '{path}'; fi",
                path = path,
                guard = guard,
                escaped = snippet.replace('\'', "'\\''"),
            );
            let _ = client
                .exec_command(
                    container_id,
                    &ExecOptions {
                        cmd: vec!["sh".to_string(), "-c".to_string(), cmd],
                        user: Some("root".to_string()),
                        working_dir: None,
                        env: None,
                    },
                )
                .await;
        }
    }
}

pub async fn inject_cella_path(
    client: &dyn ContainerBackend,
    container_id: &str,
    remote_user: &str,
) {
    let home = if remote_user == "root" {
        "/root".to_string()
    } else {
        format!("/home/{remote_user}")
    };

    // PATH_SNIPPETS are POSIX-safe → all profiles including .profile.
    inject_snippets(
        client,
        container_id,
        &home,
        PATH_SNIPPETS,
        &[".bashrc", ".zshrc", ".profile"],
    )
    .await;
    // TITLE_SNIPPETS contain zsh-specific syntax (precmd_functions+=) that
    // dash cannot parse even inside a dead branch → .bashrc/.zshrc only.
    inject_snippets(
        client,
        container_id,
        &home,
        TITLE_SNIPPETS,
        &[".bashrc", ".zshrc"],
    )
    .await;
    // Completions are versioned rather than additive: a changed snippet must
    // replace the old block, not sit beside it. Same profiles as TITLE_SNIPPETS
    // — dash reads `.profile` and has no completion system to feed.
    inject_managed_block(
        client,
        container_id,
        &home,
        COMPLETION_SNIPPETS,
        &[".bashrc", ".zshrc"],
    )
    .await;
}

/// Append a snippet, first deleting any earlier version of the same block.
///
/// [`inject_snippets`] is purely additive — its guard means "already present,
/// leave it alone", so a container that has an old block never gets the new
/// one. This variant guards on the *versioned* opening marker and, when it does
/// not match, deletes everything between the unversioned start and end markers
/// before appending. Bumping the version in the guard therefore replaces the
/// block in every existing container.
///
/// Best-effort, like its sibling: if the rewrite cannot run the append still
/// does, and the result is the additive behaviour we had before. Errors are
/// discarded either way.
async fn inject_managed_block(
    client: &dyn ContainerBackend,
    container_id: &str,
    home: &str,
    snippets: &[(&str, &str)],
    profiles: &[&str],
) {
    for (guard, snippet) in snippets {
        for profile in profiles {
            let cmd = managed_block_command(&format!("{home}/{profile}"), guard, snippet);
            let _ = client
                .exec_command(
                    container_id,
                    &ExecOptions {
                        cmd: vec!["sh".to_string(), "-c".to_string(), cmd],
                        user: Some("root".to_string()),
                        working_dir: None,
                        env: None,
                    },
                )
                .await;
        }
    }
}

/// Strip a complete `start … end` block, buffering so an *unterminated* one is
/// put back verbatim.
///
/// The obvious `sed -i '/start/,/end/d'` is wrong twice over. A sed range whose
/// closing address never matches runs to end of file, so an rc file carrying a
/// half-written block — a previous injection that was cut short, or a stray end
/// marker above the start — loses every line the user wrote after it, deleted
/// by a command running as root. And `sed -i` rewrites through a temp file, so
/// the container user's `.bashrc` would come back owned by root.
///
/// awk is POSIX-mandated and present in busybox, so this costs no portability.
const STRIP_BLOCK_AWK: &str = concat!(
    r#"skip { buf = buf $0 "\n"; if (index($0, e)) { skip = 0; buf = "" } next } "#,
    r#"index($0, s) { skip = 1; buf = $0 "\n"; next } "#,
    r#"{ print } "#,
    r#"END { if (skip) printf "%s", buf }"#,
);

/// The `sh -c` program [`inject_managed_block`] runs against one profile.
///
/// Split out so the shell logic can be exercised against real files instead of
/// only read — the strip-then-append sequence is the part that can corrupt an
/// rc file if it is wrong.
///
/// The rewrite is `awk … > tmp && cat tmp > path`, not `mv`: `cat` into the
/// existing file keeps its inode, mode and owner, which matters because this
/// runs as root against a file the container user owns. If awk is missing or
/// fails, `&&` leaves the original untouched and the append still happens —
/// degrading to the additive behaviour of [`inject_snippets`].
fn managed_block_command(path: &str, guard: &str, snippet: &str) -> String {
    format!(
        "if [ -f '{path}' ] && ! grep -q '{guard}' '{path}'; then \
         awk -v s='{start}' -v e='{end}' '{prog}' '{path}' > '{path}.cella-tmp' 2>/dev/null \
         && cat '{path}.cella-tmp' > '{path}'; \
         rm -f '{path}.cella-tmp'; \
         printf '%s\\n' '{escaped}' >> '{path}'; fi",
        start = COMPLETION_BLOCK_START_PATTERN,
        end = COMPLETION_BLOCK_END_PATTERN,
        prog = STRIP_BLOCK_AWK,
        escaped = snippet.replace('\'', "'\\''"),
    )
}

/// Opening marker of *any* version of the block.
const COMPLETION_BLOCK_START_PATTERN: &str = "# >>> cella shell completion";
/// Closing marker.
const COMPLETION_BLOCK_END_PATTERN: &str = "# <<< cella shell completion";

/// Source the generated completion scripts from the shells that have one.
///
/// The snippet is as thin as physically possible, and that is architectural.
/// Even with a versioned guard, the block in a given container is rewritten
/// only when the version changes; the script files under `/cella/share` are
/// replaced wholesale on every volume repopulation. So all logic — including
/// zsh's `compinit` guard — belongs in the versioned files, and this holds
/// nothing but shell detection and a `.`.
const COMPLETION_SNIPPETS: &[(&str, &str)] = &[(
    "# >>> cella shell completion v1 >>>",
    r#"
# >>> cella shell completion v1 >>>
if [ -n "$BASH_VERSION" ]; then
    [ -r /cella/share/completions/cella.bash ] && . /cella/share/completions/cella.bash
elif [ -n "$ZSH_VERSION" ]; then
    [ -r /cella/share/completions/cella.zsh ] && . /cella/share/completions/cella.zsh
fi
# <<< cella shell completion <<<
"#,
)];

const PATH_SNIPPETS: &[(&str, &str)] = &[
    (
        "# cella CLI",
        r#"
# cella CLI (in-container worktree commands)
if [ -d /cella/bin ]; then
    case ":$PATH:" in
        *":/cella/bin:"*) ;;
        *) export PATH="/cella/bin:$PATH" ;;
    esac
fi
"#,
    ),
    (
        "# cella user-local bin",
        r#"
# cella user-local bin (curl|bash installers: claude, uv, rustup, etc.)
if [ -d "$HOME/.local/bin" ]; then
    case ":$PATH:" in
        *":$HOME/.local/bin:"*) ;;
        *) export PATH="$HOME/.local/bin:$PATH" ;;
    esac
fi
"#,
    ),
];

const TITLE_SNIPPETS: &[(&str, &str)] = &[(
    "# cella terminal title",
    r#"
# cella terminal title (re-set title after each command for WezTerm compat)
if [ -n "$CELLA_TITLE" ]; then
    if [ -n "$BASH" ]; then
        __cella_title() { printf '\033]0;%s\007' "$CELLA_TITLE"; }
        PROMPT_COMMAND="${PROMPT_COMMAND:+$PROMPT_COMMAND;}__cella_title"
    elif [ -n "$ZSH_VERSION" ]; then
        __cella_title() { printf '\033]0;%s\007' "$CELLA_TITLE"; }
        precmd_functions+=(__cella_title)
    fi
fi
"#,
)];

/// Seed gh CLI credentials into a container.
///
/// Extracts tokens from the host's gh CLI and uploads `hosts.yml` and
/// `config.yml` into the container. Skips silently if gh is not
/// installed/authenticated or if credentials already exist in the container.
pub async fn seed_gh_credentials(
    client: &dyn ContainerBackend,
    container_id: &str,
    workspace_root: &std::path::Path,
    remote_user: &str,
) {
    let config_dir = cella_env::gh_credential::gh_config_dir_for_user(remote_user);

    if config_exists_in_container(
        client,
        container_id,
        remote_user,
        &cella_env::gh_credential::gh_config_exists_in_container(&config_dir),
    )
    .await
    {
        debug!("gh credentials already present in container, skipping seed");
        return;
    }

    let Some(gh_creds) =
        cella_env::gh_credential::prepare_gh_credentials(workspace_root, remote_user)
    else {
        return;
    };

    if upload_to_container(
        client,
        container_id,
        remote_user,
        &config_dir,
        &gh_creds.file_uploads,
        "gh config",
    )
    .await
    {
        debug!("Seeded gh CLI credentials into container");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── resolve_remote_user ──────────────────────────────────────────────

    #[test]
    fn resolve_remote_user_from_config() {
        let config = json!({"remoteUser": "vscode"});
        let result = resolve_remote_user(&config, None, "root");
        assert_eq!(result, "vscode");
    }

    #[test]
    fn resolve_remote_user_from_container_user() {
        let config = json!({"containerUser": "node"});
        let result = resolve_remote_user(&config, None, "root");
        assert_eq!(result, "node");
    }

    #[test]
    fn resolve_remote_user_from_image_metadata() {
        let config = json!({});
        let meta = cella_features::ImageMetadataUserInfo {
            remote_user: Some("devuser".to_string()),
            container_user: None,
        };
        let result = resolve_remote_user(&config, Some(&meta), "root");
        assert_eq!(result, "devuser");
    }

    #[test]
    fn resolve_remote_user_fallback() {
        let config = json!({});
        let result = resolve_remote_user(&config, None, "root");
        assert_eq!(result, "root");
    }

    #[test]
    fn resolve_remote_user_priority_order() {
        let config = json!({"remoteUser": "winner", "containerUser": "loser"});
        let meta = cella_features::ImageMetadataUserInfo {
            remote_user: Some("also-loser".to_string()),
            container_user: Some("yet-another-loser".to_string()),
        };
        let result = resolve_remote_user(&config, Some(&meta), "fallback");
        assert_eq!(result, "winner");
    }

    #[test]
    fn resolve_remote_user_container_user_beats_metadata() {
        let config = json!({"containerUser": "config-user"});
        let meta = cella_features::ImageMetadataUserInfo {
            remote_user: Some("meta-remote".to_string()),
            container_user: Some("meta-container".to_string()),
        };
        let result = resolve_remote_user(&config, Some(&meta), "fallback");
        assert_eq!(result, "config-user");
    }

    #[test]
    fn resolve_remote_user_metadata_container_user_over_fallback() {
        let config = json!({});
        let meta = cella_features::ImageMetadataUserInfo {
            remote_user: None,
            container_user: Some("meta-container".to_string()),
        };
        let result = resolve_remote_user(&config, Some(&meta), "fallback");
        assert_eq!(result, "meta-container");
    }

    #[test]
    fn resolve_remote_user_non_string_values_ignored() {
        let config = json!({"remoteUser": 42, "containerUser": true});
        let result = resolve_remote_user(&config, None, "fallback");
        assert_eq!(result, "fallback");
    }

    // ── PATH_SNIPPETS ────────────────────────────────────────────────────

    #[test]
    fn path_snippets_include_cella_bin() {
        let (guard, snippet) = PATH_SNIPPETS[0];
        assert_eq!(guard, "# cella CLI");
        assert!(snippet.contains("/cella/bin"));
    }

    #[test]
    fn path_snippets_include_user_local_bin() {
        let (guard, snippet) = PATH_SNIPPETS[1];
        assert_eq!(guard, "# cella user-local bin");
        assert!(snippet.contains("$HOME/.local/bin"));
    }

    #[test]
    fn path_snippets_have_unique_guards() {
        let guards: Vec<&str> = PATH_SNIPPETS.iter().map(|(g, _)| *g).collect();
        let unique: std::collections::HashSet<&str> = guards.iter().copied().collect();
        assert_eq!(guards.len(), unique.len());
    }

    // ── TITLE_SNIPPETS ────────────────────────────────────────────────────

    #[test]
    fn title_snippet_is_valid_bash() {
        let (_, snippet) = TITLE_SNIPPETS[0];
        let output = std::process::Command::new("bash")
            .args(["-n", "-c", snippet])
            .output()
            .expect("bash should be available");
        assert!(
            output.status.success(),
            "TITLE_SNIPPETS must be valid bash syntax: {}",
            String::from_utf8_lossy(&output.stderr),
        );
    }

    // ── COMPLETION_SNIPPETS ───────────────────────────────────────────────

    fn shell_parses(shell: &str, snippet: &str) -> Result<(), String> {
        let output = std::process::Command::new(shell)
            .args(["-n", "-c", snippet])
            .output()
            .map_err(|e| format!("{shell} must be available: {e}"))?;
        if output.status.success() {
            return Ok(());
        }
        Err(String::from_utf8_lossy(&output.stderr).into_owned())
    }

    #[test]
    fn completion_snippet_is_valid_bash() {
        let (_, snippet) = COMPLETION_SNIPPETS[0];
        shell_parses("bash", snippet).expect("COMPLETION_SNIPPETS must be valid bash");
    }

    #[test]
    fn completion_snippet_is_valid_zsh() {
        let (_, snippet) = COMPLETION_SNIPPETS[0];
        shell_parses("zsh", snippet).expect("COMPLETION_SNIPPETS must be valid zsh");
    }

    /// A container with neither bash nor zsh still parses the block: both
    /// branches are dead, but a parse error in an rc file is fatal to the
    /// shell, so the body has to be POSIX-clean regardless.
    #[test]
    fn completion_snippet_is_valid_posix_sh() {
        let (_, snippet) = COMPLETION_SNIPPETS[0];
        for shell in ["sh", "dash"] {
            match shell_parses(shell, snippet) {
                Ok(()) => {}
                // Absent shell: nothing to assert, `sh` always exists.
                Err(e) if e.contains("must be available") && shell != "sh" => {}
                Err(e) => panic!("COMPLETION_SNIPPETS must parse under {shell}: {e}"),
            }
        }
    }

    /// The block is replaced rather than skipped when it changes, which only
    /// works if the guard carries a version to compare against.
    #[test]
    fn completion_guard_is_versioned() {
        assert!(COMPLETION_SNIPPETS[0].0.contains("v1"));
    }

    /// `inject_managed_block` deletes from the opening marker to the closing
    /// one, so both must be present and the guard must be the opening marker.
    #[test]
    fn completion_snippet_is_delimited_by_its_guard() {
        let (guard, snippet) = COMPLETION_SNIPPETS[0];
        assert!(snippet.contains(guard), "snippet must open with its guard");
        assert!(
            snippet.contains(COMPLETION_BLOCK_END_PATTERN),
            "snippet must close with the end marker"
        );
    }

    /// The snippet only sources; every piece of logic lives in the files under
    /// `/cella/share`, which are replaced on every volume repopulation. A block
    /// already written into an rc file is only rewritten when its version bumps.
    #[test]
    fn completion_snippet_sources_both_generated_scripts() {
        let (_, snippet) = COMPLETION_SNIPPETS[0];
        assert!(snippet.contains("/cella/share/completions/cella.bash"));
        assert!(snippet.contains("/cella/share/completions/cella.zsh"));
        // `-r`, not `-f`: volume population failures are warned and tolerated,
        // so an unreadable path must be a no-op rather than an error.
        assert!(snippet.contains("[ -r "), "must guard on readability");
    }

    /// The whole point of the versioned guard: run the real `sh` program
    /// against real files and check what lands in them.
    fn run_injection(rc: &std::path::Path, guard: &str, snippet: &str) {
        let cmd = managed_block_command(&rc.to_string_lossy(), guard, snippet);
        let status = std::process::Command::new("sh")
            .args(["-c", &cmd])
            .status()
            .expect("sh must be available");
        assert!(status.success(), "injection command failed: {cmd}");
    }

    fn blocks_in(rc: &std::path::Path) -> usize {
        std::fs::read_to_string(rc)
            .unwrap()
            .matches(COMPLETION_BLOCK_START_PATTERN)
            .count()
    }

    #[test]
    fn injection_appends_then_stays_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let rc = dir.path().join(".bashrc");
        std::fs::write(&rc, "export EDITOR=vi\n").unwrap();
        let (guard, snippet) = COMPLETION_SNIPPETS[0];

        run_injection(&rc, guard, snippet);
        assert_eq!(blocks_in(&rc), 1, "first run must append the block");

        run_injection(&rc, guard, snippet);
        assert_eq!(blocks_in(&rc), 1, "same version must not append again");

        let after = std::fs::read_to_string(&rc).unwrap();
        assert!(after.starts_with("export EDITOR=vi\n"), "{after}");
        assert!(
            after.contains("/cella/share/completions/cella.bash"),
            "{after}"
        );
    }

    /// A bumped version must *replace* the old block, not stack a second one —
    /// the failure `inject_snippets`' additive guard cannot avoid.
    #[test]
    fn injection_replaces_an_older_version() {
        let dir = tempfile::tempdir().unwrap();
        let rc = dir.path().join(".bashrc");
        let old = "# >>> cella shell completion v0 >>>\nold_and_wrong\n# <<< cella shell completion <<<\n";
        std::fs::write(&rc, format!("export EDITOR=vi\n{old}alias l=ls\n")).unwrap();
        let (guard, snippet) = COMPLETION_SNIPPETS[0];

        run_injection(&rc, guard, snippet);

        let after = std::fs::read_to_string(&rc).unwrap();
        assert_eq!(blocks_in(&rc), 1, "exactly one block must remain: {after}");
        assert!(
            !after.contains("old_and_wrong"),
            "old block survived: {after}"
        );
        assert!(after.contains("v1"), "new block missing: {after}");
        assert!(
            after.contains("export EDITOR=vi"),
            "clobbered the file: {after}"
        );
        assert!(after.contains("alias l=ls"), "clobbered the file: {after}");
    }

    /// A block whose end marker is missing — a previous write that was cut
    /// short — must be left alone, not treated as "delete to end of file".
    /// A `sed '/start/,/end/d'` range does exactly that, as root, taking every
    /// line the user wrote after it.
    #[test]
    fn an_unterminated_block_does_not_eat_the_rest_of_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let rc = dir.path().join(".bashrc");
        std::fs::write(
            &rc,
            "export EDITOR=vi\n\
             # >>> cella shell completion v0 >>>\n\
             truncated\n\
             export SECRET_TOOL=1\n\
             alias l=ls\n",
        )
        .unwrap();
        let (guard, snippet) = COMPLETION_SNIPPETS[0];

        run_injection(&rc, guard, snippet);

        let after = std::fs::read_to_string(&rc).unwrap();
        assert!(
            after.contains("export SECRET_TOOL=1"),
            "content after an unterminated block was destroyed: {after}"
        );
        assert!(after.contains("alias l=ls"), "{after}");
        assert!(after.contains("export EDITOR=vi"), "{after}");
        assert!(
            after.contains("v1"),
            "new block must still be appended: {after}"
        );
    }

    /// An end marker sitting *before* any start marker must not open a range
    /// that runs to EOF either.
    #[test]
    fn a_stray_end_marker_does_not_eat_the_rest_of_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let rc = dir.path().join(".bashrc");
        std::fs::write(
            &rc,
            "# <<< cella shell completion <<<\n\
             export KEEP_ME=1\n\
             # >>> cella shell completion v0 >>>\n\
             stale\n\
             export ALSO_KEEP=1\n",
        )
        .unwrap();
        let (guard, snippet) = COMPLETION_SNIPPETS[0];

        run_injection(&rc, guard, snippet);

        let after = std::fs::read_to_string(&rc).unwrap();
        assert!(after.contains("export KEEP_ME=1"), "{after}");
        assert!(after.contains("export ALSO_KEEP=1"), "{after}");
    }

    /// The rc file belongs to the container user; injection runs as root and
    /// must not hand ownership of the file to root by replacing it.
    #[test]
    fn injection_edits_in_place_rather_than_replacing_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let rc = dir.path().join(".bashrc");
        let old = "# >>> cella shell completion v0 >>>\nstale\n# <<< cella shell completion <<<\n";
        std::fs::write(&rc, format!("export EDITOR=vi\n{old}")).unwrap();
        let before = std::fs::metadata(&rc).unwrap();
        let (guard, snippet) = COMPLETION_SNIPPETS[0];

        run_injection(&rc, guard, snippet);

        let after = std::fs::metadata(&rc).unwrap();
        assert_eq!(
            std::os::unix::fs::MetadataExt::ino(&before),
            std::os::unix::fs::MetadataExt::ino(&after),
            "the rc file must keep its inode, ownership and mode"
        );
    }

    /// Absent rc file: nothing is created, nothing is printed, exit 0. Such a
    /// container gets no PATH block either, so `cella` is not on PATH there.
    #[test]
    fn injection_skips_a_missing_profile() {
        let dir = tempfile::tempdir().unwrap();
        let rc = dir.path().join(".zshrc");
        let (guard, snippet) = COMPLETION_SNIPPETS[0];

        run_injection(&rc, guard, snippet);

        assert!(!rc.exists(), "must not create the profile");
    }

    /// Fallback on an image with no usable `awk`: the strip cannot run, the
    /// append still does, and the result degrades to exactly the additive
    /// behaviour `inject_snippets` already has — the stale block survives
    /// beside the new one. Never a corrupted, truncated, or emptied file.
    ///
    /// Seeding a stale block first is what makes this test able to tell the
    /// stub `awk` from the real one: with a working `awk` the answer is one
    /// block, with a broken one it is two.
    #[test]
    fn a_missing_awk_degrades_to_appending() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let stub = bin.join("awk");
        std::fs::write(&stub, "#!/bin/sh\necho 'awk: not found' >&2\nexit 127\n").unwrap();
        std::process::Command::new("chmod")
            .args(["+x", &stub.to_string_lossy()])
            .status()
            .unwrap();

        let rc = dir.path().join(".bashrc");
        let stale =
            "# >>> cella shell completion v0 >>>\nstale\n# <<< cella shell completion <<<\n";
        std::fs::write(&rc, format!("export EDITOR=vi\n{stale}")).unwrap();
        let (guard, snippet) = COMPLETION_SNIPPETS[0];

        let cmd = managed_block_command(&rc.to_string_lossy(), guard, snippet);
        let status = std::process::Command::new("sh")
            .args(["-c", &cmd])
            // Prepended, not replaced: `sh`, `grep`, `cat`, `rm` and `printf`
            // must still resolve — only `awk` is shadowed.
            .env(
                "PATH",
                format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
            )
            .status()
            .expect("sh must be available");
        assert!(status.success(), "a missing awk must not fail the command");

        let after = std::fs::read_to_string(&rc).unwrap();
        assert_eq!(
            blocks_in(&rc),
            2,
            "the stub awk must have been used, leaving both blocks: {after}"
        );
        assert!(after.contains("stale"), "stale block must survive: {after}");
        assert!(after.contains("v1"), "new block must be appended: {after}");
        assert!(
            after.contains("export EDITOR=vi"),
            "clobbered the file: {after}"
        );
    }

    /// The rewrite must not leave its scratch file behind in the user's home.
    #[test]
    fn injection_leaves_no_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        let rc = dir.path().join(".bashrc");
        std::fs::write(&rc, "export EDITOR=vi\n").unwrap();
        let (guard, snippet) = COMPLETION_SNIPPETS[0];

        run_injection(&rc, guard, snippet);

        let leftovers: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
            .filter(|n| n != ".bashrc")
            .collect();
        assert!(
            leftovers.is_empty(),
            "scratch files left behind: {leftovers:?}"
        );
    }

    #[test]
    fn completion_snippets_have_unique_guards() {
        let guards: Vec<&str> = PATH_SNIPPETS
            .iter()
            .chain(TITLE_SNIPPETS)
            .chain(COMPLETION_SNIPPETS)
            .map(|(g, _)| *g)
            .collect();
        let unique: std::collections::HashSet<&str> = guards.iter().copied().collect();
        assert_eq!(guards.len(), unique.len(), "guards must not collide");
    }

    // ── map_env_object ───────────────────────────────────────────────────

    #[test]
    fn map_env_object_basic() {
        let val = json!({"FOO": "bar"});
        let result = map_env_object(Some(&val));
        assert_eq!(result, vec!["FOO=bar"]);
    }

    #[test]
    fn map_env_object_null_values() {
        let val = json!({"KEY": null});
        let result = map_env_object(Some(&val));
        assert_eq!(result, vec!["KEY="]);
    }

    #[test]
    fn map_env_object_none_input() {
        let result = map_env_object(None);
        assert!(result.is_empty());
    }

    #[test]
    fn map_env_object_non_object_value() {
        let val = json!("not an object");
        let result = map_env_object(Some(&val));
        assert!(result.is_empty());
    }

    #[test]
    fn map_env_object_multiple_keys() {
        let val = json!({"A": "1", "B": "2", "C": "3"});
        let result = map_env_object(Some(&val));
        assert_eq!(result.len(), 3);
        // Object iteration order is consistent within serde_json
        for item in &result {
            assert!(item.contains('='));
        }
    }

    #[test]
    fn map_env_object_empty_object() {
        let val = json!({});
        let result = map_env_object(Some(&val));
        assert!(result.is_empty());
    }

    #[test]
    fn map_env_object_integer_value_treated_as_empty() {
        let val = json!({"PORT": 8080});
        let result = map_env_object(Some(&val));
        assert_eq!(result, vec!["PORT="]);
    }

    // ── convert_uploads ──────────────────────────────────────────────────

    #[test]
    fn convert_uploads_basic() {
        let uploads = vec![cella_env::FileUpload {
            container_path: "/home/user/.config/test".to_string(),
            content: b"hello".to_vec(),
            mode: 0o644,
        }];
        let result = convert_uploads(&uploads);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].path, "/home/user/.config/test");
        assert_eq!(result[0].content, b"hello");
        assert_eq!(result[0].mode, 0o644);
    }

    #[test]
    fn convert_uploads_empty() {
        let result = convert_uploads(&[]);
        assert!(result.is_empty());
    }

    #[test]
    fn convert_uploads_multiple() {
        let uploads = vec![
            cella_env::FileUpload {
                container_path: "/a".to_string(),
                content: b"content-a".to_vec(),
                mode: 0o600,
            },
            cella_env::FileUpload {
                container_path: "/b".to_string(),
                content: b"content-b".to_vec(),
                mode: 0o755,
            },
        ];
        let result = convert_uploads(&uploads);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].path, "/a");
        assert_eq!(result[0].mode, 0o600);
        assert_eq!(result[1].path, "/b");
        assert_eq!(result[1].mode, 0o755);
    }

    #[test]
    fn convert_uploads_preserves_binary_content() {
        let uploads = vec![cella_env::FileUpload {
            container_path: "/bin/key".to_string(),
            content: vec![0x00, 0xFF, 0xDE, 0xAD],
            mode: 0o400,
        }];
        let result = convert_uploads(&uploads);
        assert_eq!(result[0].content, vec![0x00, 0xFF, 0xDE, 0xAD]);
    }

    // ── run_host_command ─────────────────────────────────────────────────

    #[test]
    fn run_host_command_string_form() {
        let cmd = json!("true");
        let result = run_host_command("test", &cmd);
        assert!(result.is_ok());
    }

    #[test]
    fn run_host_command_array_form() {
        let cmd = json!(["echo", "hello"]);
        let result = run_host_command("test", &cmd);
        assert!(result.is_ok());
    }

    #[test]
    fn run_host_command_object_form() {
        let cmd = json!({"step1": "true", "step2": ["echo", "done"]});
        let result = run_host_command("test", &cmd);
        assert!(result.is_ok());
    }

    #[test]
    fn run_host_command_failing_string() {
        let cmd = json!("false");
        let result = run_host_command("test", &cmd);
        assert!(result.is_err());
    }

    #[test]
    fn run_host_command_object_form_surfaces_failure() {
        let cmd = json!({"ok": "true", "bad": "false"});
        assert!(run_host_command("test", &cmd).is_err());
    }

    #[test]
    fn run_host_command_object_form_runs_all_despite_failure() {
        // A slow sibling must run to completion even though another entry fails
        // (parallel + allSettled semantics, not cancel-on-first-error).
        let marker = std::env::temp_dir().join(format!(
            "cella-lifecycle-parallel-{}.marker",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&marker);
        let touch = format!("sleep 0.3 && touch '{}'", marker.display());
        let cmd = json!({"slow": touch, "fail": "false"});

        let result = run_host_command("test", &cmd);
        assert!(result.is_err(), "the failing entry must surface an error");
        assert!(
            marker.exists(),
            "the slow sibling must complete despite the failure (not be cancelled)"
        );
        let _ = std::fs::remove_file(&marker);
    }

    #[test]
    fn run_host_command_null_is_noop() {
        let cmd = json!(null);
        let result = run_host_command("test", &cmd);
        assert!(result.is_ok());
    }

    #[test]
    fn run_host_command_bool_is_noop() {
        let cmd = json!(true);
        let result = run_host_command("test", &cmd);
        assert!(result.is_ok());
    }

    #[test]
    fn run_host_command_empty_array() {
        let cmd = json!([]);
        let result = run_host_command("test", &cmd);
        assert!(result.is_ok());
    }

    #[test]
    fn run_host_command_empty_object() {
        let cmd = json!({});
        let result = run_host_command("test", &cmd);
        assert!(result.is_ok());
    }

    #[test]
    fn run_host_command_object_with_failing_step() {
        let cmd = json!({"step1": "false"});
        let result = run_host_command("test", &cmd);
        assert!(result.is_err());
    }

    #[test]
    fn run_host_command_object_with_array_value() {
        let cmd = json!({"build": ["echo", "building"]});
        let result = run_host_command("test", &cmd);
        assert!(result.is_ok());
    }

    #[test]
    fn run_host_command_object_ignores_non_string_non_array() {
        let cmd = json!({"step": 42});
        let result = run_host_command("test", &cmd);
        assert!(result.is_ok());
    }

    // ── TITLE_SNIPPETS ──────────────────────────────────────────────

    #[test]
    fn title_snippet_contains_prompt_command_for_bash() {
        let (_, snippet) = TITLE_SNIPPETS[0];
        assert!(snippet.contains("PROMPT_COMMAND"));
        assert!(snippet.contains("CELLA_TITLE"));
    }

    #[test]
    fn title_snippet_contains_precmd_for_zsh() {
        let (_, snippet) = TITLE_SNIPPETS[0];
        assert!(snippet.contains("precmd_functions"));
        assert!(snippet.contains("ZSH_VERSION"));
    }

    #[test]
    fn title_snippet_guard_is_idempotent_marker() {
        let (guard, _) = TITLE_SNIPPETS[0];
        assert!(guard.starts_with("# cella"));
    }

    #[test]
    fn title_snippet_only_activates_when_cella_title_set() {
        let (_, snippet) = TITLE_SNIPPETS[0];
        assert!(snippet.contains(r#"if [ -n "$CELLA_TITLE" ]"#));
    }
}
