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

/// Whether a chown descends into a directory's contents.
#[derive(Clone, Copy)]
enum Recurse {
    /// `chown -R`: the path and everything beneath it.
    Yes,
    /// The named path alone.
    No,
}

async fn chown(
    client: &dyn ContainerBackend,
    container_id: &str,
    remote_user: &str,
    path: &str,
    recurse: Recurse,
) {
    let mut cmd = vec!["chown".to_string()];
    if matches!(recurse, Recurse::Yes) {
        cmd.push("-R".to_string());
    }
    cmd.push(format!("{remote_user}:{remote_user}"));
    cmd.push(path.to_string());

    let _ = client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd,
                user: Some("root".to_string()),
                env: None,
                working_dir: None,
            },
        )
        .await;
}

/// Recursively chown a directory inside the container.
///
/// Only for trees cella itself populates. A path with host bind mounts beneath
/// it needs [`chown_path_in_container`] instead — `chown -R` has no
/// `--one-file-system`, so it descends through every mount point it meets.
pub async fn chown_in_container(
    client: &dyn ContainerBackend,
    container_id: &str,
    remote_user: &str,
    dir: &str,
) {
    chown(client, container_id, remote_user, dir, Recurse::Yes).await;
}

/// Chown a single path inside the container, leaving its contents untouched.
pub async fn chown_path_in_container(
    client: &dyn ContainerBackend,
    container_id: &str,
    remote_user: &str,
    path: &str,
) {
    chown(client, container_id, remote_user, path, Recurse::No).await;
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

/// A block appended to a shell profile, replaced wholesale when its version
/// changes.
///
/// The markers travel with the body rather than being global constants: the
/// strip step matches `start`…`end`, so a helper that hardcoded one block's
/// markers while accepting any block as a parameter would happily strip the
/// *wrong* block. Keeping them together makes that unrepresentable.
struct ManagedBlock {
    /// Versioned opening marker. Doubles as the "already current" guard.
    guard: &'static str,
    /// Opening marker without the version, matched when replacing an older
    /// block.
    start: &'static str,
    /// Closing marker.
    end: &'static str,
    /// The block itself, including both markers.
    body: &'static str,
    /// Profiles this block belongs in.
    profiles: &'static [&'static str],
}

/// `sh` fragment appending `snippet` to `path` unless `guard` is already there.
///
/// Purely additive: the guard means "present, leave it alone", so a changed
/// snippet never reaches a profile that already has the old one. That is fine
/// for [`PATH_SNIPPETS`] and [`TITLE_SNIPPETS`], whose bodies are stable, and
/// is why completions use [`managed_block_command`] instead.
fn append_if_absent_command(path: &str, guard: &str, snippet: &str) -> String {
    let path = shell_single_quote_escape(path);
    format!(
        "if [ -f '{path}' ] && ! grep -q '{guard}' '{path}'; then \
         printf '%s\\n' '{escaped}' >> '{path}'; fi",
        escaped = shell_single_quote_escape(snippet),
    )
}

/// The whole shell integration as one `sh` program.
///
/// Every block for every profile is concatenated and run in a single
/// `exec_command`. Issuing one statement per (block, profile) instead meant ten
/// Docker exec round trips — create + start + inspect, plus a process spawned
/// inside the container, each — and since injection now also runs on the
/// attach-to-running path, that cost landed on every `cella up` against a
/// container that was already up. The guards make almost all of it a no-op, but
/// a no-op still costs a full round trip to discover.
fn shell_integration_program(home: &str) -> String {
    let mut statements = Vec::new();

    // PATH_SNIPPETS are POSIX-safe → all profiles including .profile.
    for (guard, snippet) in PATH_SNIPPETS {
        for profile in [".bashrc", ".zshrc", ".profile"] {
            statements.push(append_if_absent_command(
                &format!("{home}/{profile}"),
                guard,
                snippet,
            ));
        }
    }
    // TITLE_SNIPPETS contain zsh-specific syntax (precmd_functions+=) that dash
    // cannot parse even inside a dead branch → .bashrc/.zshrc only.
    for (guard, snippet) in TITLE_SNIPPETS {
        for profile in [".bashrc", ".zshrc"] {
            statements.push(append_if_absent_command(
                &format!("{home}/{profile}"),
                guard,
                snippet,
            ));
        }
    }
    // Completions are versioned rather than additive: a changed snippet must
    // replace the old block, not sit beside it.
    for block in COMPLETION_BLOCKS {
        for profile in block.profiles {
            statements.push(managed_block_command(&format!("{home}/{profile}"), block));
        }
    }

    statements.join("\n")
}

/// Install cella's shell integration into the container's profiles: `/cella/bin`
/// and `~/.local/bin` on PATH, the terminal-title hook, and completions.
///
/// `/cella/bin` makes the cella CLI (symlinked to the agent binary)
/// discoverable. `~/.local/bin` is the XDG-standard user-local binary directory
/// used by `curl | bash` installers (Claude Code, uv, rustup, etc.) — without
/// it, tools installed by `install_tools_and_probe_env` may not appear on PATH
/// in worktree containers.
///
/// Every block carries its own idempotency guard, so this is safe to re-run and
/// also patches containers created before a given block existed. Best-effort:
/// the exec's result is discarded, as a container without the profile files is
/// a normal outcome rather than a failure.
pub async fn inject_shell_integration(
    client: &dyn ContainerBackend,
    container_id: &str,
    remote_user: &str,
) {
    let home = if remote_user == "root" {
        "/root".to_string()
    } else {
        format!("/home/{remote_user}")
    };

    let _ = client
        .exec_command(
            container_id,
            &ExecOptions {
                cmd: vec![
                    "sh".to_string(),
                    "-c".to_string(),
                    shell_integration_program(&home),
                ],
                user: Some("root".to_string()),
                working_dir: None,
                env: None,
            },
        )
        .await;
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
    // A second start marker while buffering means the block we were in was
    // never terminated. Flush it back verbatim and start over here, rather than
    // running on to this block's end marker and swallowing the user's lines
    // between the two.
    r#"skip { if (index($0, s)) { printf "%s", buf; buf = $0 "\n"; next } "#,
    r#"buf = buf $0 "\n"; if (index($0, e)) { skip = 0; buf = "" } next } "#,
    r#"index($0, s) { skip = 1; buf = $0 "\n"; next } "#,
    r#"{ print } "#,
    // Hit EOF still buffering: unterminated, so put it back untouched.
    r#"END { if (skip) printf "%s", buf }"#,
);

/// `sh` fragment replacing any earlier version of `block` in `path`.
///
/// Three deliberate choices, each guarding a way this can go wrong when run as
/// root against a file the container user owns:
///
/// * The stripped text is held in a shell variable, never a scratch file. A
///   scratch path derived from the profile (`~/.bashrc.tmp`) sits in a
///   directory that user controls, so they could pre-create it as a symlink to
///   a root-owned target and have this redirect clobber it.
/// * A symlinked profile is appended to but never rewritten, matching
///   [`append_if_absent_command`] exactly. Truncate-and-rewrite through a
///   symlink would be strictly worse than the behaviour we already had.
/// * The rewrite runs only when a block is actually present, so the common
///   first-install case never rewrites the file at all.
///
/// If `awk` is missing the command substitution fails, the `if` skips the
/// rewrite, and the append still happens — degrading to purely additive.
fn managed_block_command(path: &str, block: &ManagedBlock) -> String {
    let path = shell_single_quote_escape(path);
    format!(
        "if [ -f '{path}' ] \
         && ! {{ grep -q '{guard}' '{path}' && grep -q '{end}' '{path}'; }}; then \
         if grep -q '{start}' '{path}' && [ ! -L '{path}' ] \
         && stripped=$(awk -v s='{start}' -v e='{end}' '{prog}' '{path}' 2>/dev/null); then \
         printf '%s\\n' \"$stripped\" > '{path}'; fi; \
         printf '%s\\n' '{escaped}' >> '{path}'; fi",
        guard = block.guard,
        start = block.start,
        end = block.end,
        prog = STRIP_BLOCK_AWK,
        escaped = shell_single_quote_escape(block.body),
    )
}

/// Escape `'` so a value can sit inside single quotes in a POSIX shell string.
///
/// Applies to the *paths* as well as the bodies: `home` is built from
/// `remote_user`, which comes verbatim out of a repo's devcontainer.json with no
/// charset validation, and the program it lands in runs as root.
fn shell_single_quote_escape(value: &str) -> String {
    value.replace('\'', "'\\''")
}

/// Source the generated completion scripts from the shells that have one.
///
/// The body is as thin as physically possible, and that is architectural. Even
/// with a versioned guard, the block in a given container is rewritten only
/// when the version changes; the script files under `/cella/share` are replaced
/// wholesale on every volume repopulation. So all logic — including zsh's
/// `compinit` guard — belongs in the versioned files, and this holds nothing
/// but shell detection and a `.`.
///
/// `.profile` is excluded: dash reads it and has no completion system to feed.
const COMPLETION_BLOCKS: &[ManagedBlock] = &[ManagedBlock {
    guard: "# >>> cella shell completion v1 >>>",
    start: "# >>> cella shell completion",
    end: "# <<< cella shell completion",
    profiles: &[".bashrc", ".zshrc"],
    // `if` rather than `[ -r … ] && .` — the block is the last thing in the rc
    // file, so a false `&&` would leave `$?` = 1 in every interactive shell
    // whose container has no completion scripts, and prompts that render the
    // last exit status would show a permanent error.
    body: r#"
# >>> cella shell completion v1 >>>
if [ -n "$BASH_VERSION" ]; then
    if [ -r /cella/share/completions/cella.bash ]; then
        . /cella/share/completions/cella.bash
    fi
elif [ -n "$ZSH_VERSION" ]; then
    if [ -r /cella/share/completions/cella.zsh ]; then
        . /cella/share/completions/cella.zsh
    fi
fi
# <<< cella shell completion <<<
"#,
}];

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

    // ── COMPLETION_BLOCKS ────────────────────────────────────────────────

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
        let snippet = COMPLETION_BLOCKS[0].body;
        shell_parses("bash", snippet).expect("COMPLETION_BLOCKS must be valid bash");
    }

    #[test]
    fn completion_snippet_is_valid_zsh() {
        let snippet = COMPLETION_BLOCKS[0].body;
        shell_parses("zsh", snippet).expect("COMPLETION_BLOCKS must be valid zsh");
    }

    /// A container with neither bash nor zsh still parses the block: both
    /// branches are dead, but a parse error in an rc file is fatal to the
    /// shell, so the body has to be POSIX-clean regardless.
    #[test]
    fn completion_snippet_is_valid_posix_sh() {
        let snippet = COMPLETION_BLOCKS[0].body;
        for shell in ["sh", "dash"] {
            match shell_parses(shell, snippet) {
                Ok(()) => {}
                // Absent shell: nothing to assert, `sh` always exists.
                Err(e) if e.contains("must be available") && shell != "sh" => {}
                Err(e) => panic!("COMPLETION_BLOCKS must parse under {shell}: {e}"),
            }
        }
    }

    /// The block is replaced rather than skipped when it changes, which only
    /// works if the guard carries a version to compare against.
    #[test]
    fn completion_guard_is_versioned() {
        assert!(COMPLETION_BLOCKS[0].guard.contains("v1"));
    }

    /// `managed_block_command` strips from the opening marker to the closing
    /// one, so both must be present and the guard must extend the opening one.
    #[test]
    fn completion_snippet_is_delimited_by_its_guard() {
        let block = &COMPLETION_BLOCKS[0];
        assert!(
            block.body.contains(block.guard),
            "body must open with its versioned guard"
        );
        assert!(
            block.body.contains(block.start),
            "the versioned guard must extend the unversioned start marker"
        );
        assert!(
            block.body.contains(block.end),
            "body must close with the end marker"
        );
    }

    /// The snippet only sources; every piece of logic lives in the files under
    /// `/cella/share`, which are replaced on every volume repopulation. A block
    /// already written into an rc file is only rewritten when its version bumps.
    #[test]
    fn completion_snippet_sources_both_generated_scripts() {
        let snippet = COMPLETION_BLOCKS[0].body;
        assert!(snippet.contains("/cella/share/completions/cella.bash"));
        assert!(snippet.contains("/cella/share/completions/cella.zsh"));
        // `-r`, not `-f`: volume population failures are warned and tolerated,
        // so an unreadable path must be a no-op rather than an error.
        assert!(snippet.contains("[ -r "), "must guard on readability");
    }

    /// The block is the last thing in the rc file. If it ends on a false test,
    /// every interactive shell in a container without the completion scripts
    /// starts with `$?` = 1, and any prompt showing the last status shows an
    /// error the user cannot explain.
    #[test]
    fn the_completion_block_leaves_a_clean_exit_status() {
        let body = COMPLETION_BLOCKS[0]
            .body
            .replace("/cella/share/completions", "/nonexistent");
        for shell in ["bash", "zsh", "sh"] {
            let out = std::process::Command::new(shell)
                .args(["-c", &format!("{body}\nexit $?")])
                .status()
                .unwrap_or_else(|e| panic!("{shell} must be available: {e}"));
            assert!(
                out.success(),
                "{shell}: a missing completion script must leave $? = 0"
            );
        }
    }

    /// `remote_user` is read verbatim from a repo's devcontainer.json and ends
    /// up inside single quotes in a program that runs as root. A quote in it
    /// must terminate as data, not close the quoting and start a command.
    #[test]
    fn a_quote_in_the_home_path_cannot_break_out() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("pwned");
        // The shape a malicious `"remoteUser"` would take.
        let hostile = format!(
            "{}/x'; touch '{}'; '",
            dir.path().display(),
            marker.display()
        );

        let program = shell_integration_program(&hostile);
        let status = std::process::Command::new("sh")
            .args(["-c", &program])
            .status()
            .expect("sh must be available");

        assert!(status.success(), "program must still parse: {program}");
        assert!(
            !marker.exists(),
            "injected command executed — the path was not escaped:\n{program}"
        );
    }

    /// The batched program is what actually runs in the container: every block,
    /// every profile, one `sh -c`. Driving it against real files is the only
    /// check that the concatenation of ten `if … fi` statements still parses
    /// and still does each job.
    #[test]
    fn one_program_injects_every_block_into_every_profile() {
        let dir = tempfile::tempdir().unwrap();
        for profile in [".bashrc", ".zshrc", ".profile"] {
            std::fs::write(dir.path().join(profile), "export EDITOR=vi\n").unwrap();
        }
        let program = shell_integration_program(&dir.path().to_string_lossy());

        let status = std::process::Command::new("sh")
            .args(["-c", &program])
            .status()
            .expect("sh must be available");
        assert!(status.success(), "program failed: {program}");

        let read = |name: &str| std::fs::read_to_string(dir.path().join(name)).unwrap();

        // PATH blocks reach every profile, including `.profile`.
        for profile in [".bashrc", ".zshrc", ".profile"] {
            let text = read(profile);
            for (guard, _) in PATH_SNIPPETS {
                assert!(text.contains(guard), "{profile} missing `{guard}`");
            }
        }
        // Title and completion blocks stay out of `.profile`, which dash reads.
        for profile in [".bashrc", ".zshrc"] {
            let text = read(profile);
            assert!(
                text.contains(TITLE_SNIPPETS[0].0),
                "{profile} missing title"
            );
            assert!(
                text.contains(COMPLETION_BLOCKS[0].guard),
                "{profile} missing completions"
            );
        }
        let profile_text = read(".profile");
        assert!(
            !profile_text.contains(COMPLETION_BLOCKS[0].guard),
            "completions must not reach .profile: {profile_text}"
        );
        assert!(
            !profile_text.contains(TITLE_SNIPPETS[0].0),
            "title hook must not reach .profile: {profile_text}"
        );
    }

    /// Re-running must change nothing — it runs on every `cella up`.
    #[test]
    fn the_program_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        for profile in [".bashrc", ".zshrc", ".profile"] {
            std::fs::write(dir.path().join(profile), "export EDITOR=vi\n").unwrap();
        }
        let program = shell_integration_program(&dir.path().to_string_lossy());
        let run = || {
            std::process::Command::new("sh")
                .args(["-c", &program])
                .status()
                .expect("sh must be available")
        };

        assert!(run().success());
        let first: Vec<String> = [".bashrc", ".zshrc", ".profile"]
            .iter()
            .map(|p| std::fs::read_to_string(dir.path().join(p)).unwrap())
            .collect();
        assert!(run().success());
        let second: Vec<String> = [".bashrc", ".zshrc", ".profile"]
            .iter()
            .map(|p| std::fs::read_to_string(dir.path().join(p)).unwrap())
            .collect();

        assert_eq!(first, second, "a second run must be a no-op");
    }

    /// The batched program has to parse as POSIX sh — it is handed to `sh -c`,
    /// and a syntax error anywhere kills every block, not just one.
    #[test]
    fn the_program_is_valid_posix_sh() {
        let program = shell_integration_program("/home/dev");
        shell_parses("sh", &program).expect("shell_integration_program must parse under sh");
    }

    /// The whole point of the versioned guard: run the real `sh` program
    /// against real files and check what lands in them.
    fn run_injection(rc: &std::path::Path, block: &ManagedBlock) {
        let cmd = managed_block_command(&rc.to_string_lossy(), block);
        let status = std::process::Command::new("sh")
            .args(["-c", &cmd])
            .status()
            .expect("sh must be available");
        assert!(status.success(), "injection command failed: {cmd}");
    }

    fn blocks_in(rc: &std::path::Path) -> usize {
        std::fs::read_to_string(rc)
            .unwrap()
            .matches(COMPLETION_BLOCKS[0].start)
            .count()
    }

    #[test]
    fn injection_appends_then_stays_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let rc = dir.path().join(".bashrc");
        std::fs::write(&rc, "export EDITOR=vi\n").unwrap();
        let block = &COMPLETION_BLOCKS[0];

        run_injection(&rc, block);
        assert_eq!(blocks_in(&rc), 1, "first run must append the block");

        run_injection(&rc, block);
        assert_eq!(blocks_in(&rc), 1, "same version must not append again");

        let after = std::fs::read_to_string(&rc).unwrap();
        assert!(after.starts_with("export EDITOR=vi\n"), "{after}");
        assert!(
            after.contains("/cella/share/completions/cella.bash"),
            "{after}"
        );
    }

    /// A bumped version must *replace* the old block, not stack a second one —
    /// the failure an additive guard cannot avoid.
    #[test]
    fn injection_replaces_an_older_version() {
        let dir = tempfile::tempdir().unwrap();
        let rc = dir.path().join(".bashrc");
        let old = "# >>> cella shell completion v0 >>>\nold_and_wrong\n# <<< cella shell completion <<<\n";
        std::fs::write(&rc, format!("export EDITOR=vi\n{old}alias l=ls\n")).unwrap();
        let block = &COMPLETION_BLOCKS[0];

        run_injection(&rc, block);

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
        let block = &COMPLETION_BLOCKS[0];

        run_injection(&rc, block);

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

    /// A block whose opening marker landed but whose body did not — an append
    /// cut short — must be repaired, not treated as already current. Guarding
    /// on the opening marker alone leaves completions permanently broken in
    /// that container until the version happens to change.
    #[test]
    fn a_half_written_current_block_is_repaired() {
        let dir = tempfile::tempdir().unwrap();
        let rc = dir.path().join(".bashrc");
        let block = &COMPLETION_BLOCKS[0];
        std::fs::write(&rc, format!("export EDITOR=vi\n{}\n", block.guard)).unwrap();

        run_injection(&rc, block);

        let after = std::fs::read_to_string(&rc).unwrap();
        assert!(
            after.contains("/cella/share/completions/cella.bash"),
            "the truncated block must be completed: {after}"
        );
        assert!(
            after.contains(block.end),
            "closing marker must be present: {after}"
        );
        assert!(after.contains("export EDITOR=vi"), "{after}");
    }

    /// The lifecycle this code produces on its own: a write cut short leaves an
    /// unterminated block, the next run appends a well-formed one after the
    /// user's lines, and the version bump after that has to strip only the
    /// second. Buffering to the *next* end marker anywhere in the file instead
    /// swallows everything between the two — as root, silently.
    #[test]
    fn a_stale_unterminated_block_does_not_swallow_a_later_one() {
        let dir = tempfile::tempdir().unwrap();
        let rc = dir.path().join(".bashrc");
        std::fs::write(
            &rc,
            "export EDITOR=vi\n\
             # >>> cella shell completion v0 >>>\n\
             truncated\n\
             export SECRET_TOOL=1\n\
             alias l=ls\n\
             # >>> cella shell completion v0 >>>\n\
             current\n\
             # <<< cella shell completion <<<\n\
             export TAIL=1\n",
        )
        .unwrap();
        let block = &COMPLETION_BLOCKS[0];

        run_injection(&rc, block);

        let after = std::fs::read_to_string(&rc).unwrap();
        for line in [
            "export EDITOR=vi",
            "export SECRET_TOOL=1",
            "alias l=ls",
            "export TAIL=1",
        ] {
            assert!(after.contains(line), "`{line}` was swallowed: {after}");
        }
        assert!(
            !after.contains("current"),
            "the well-formed block must still be replaced: {after}"
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
        let block = &COMPLETION_BLOCKS[0];

        run_injection(&rc, block);

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
        let block = &COMPLETION_BLOCKS[0];

        run_injection(&rc, block);

        let after = std::fs::metadata(&rc).unwrap();
        assert_eq!(
            std::os::unix::fs::MetadataExt::ino(&before),
            std::os::unix::fs::MetadataExt::ino(&after),
            "the rc file must keep its inode, ownership and mode"
        );
    }

    /// The rewrite must never route through a predictable scratch path in a
    /// directory the container user owns: they could pre-create it as a symlink
    /// to a root-owned file and have our root-run redirect clobber the target.
    #[test]
    fn injection_never_writes_a_predictable_scratch_path() {
        let cmd = managed_block_command("/home/dev/.bashrc", &COMPLETION_BLOCKS[0]);
        assert!(
            !cmd.contains(".bashrc."),
            "no derived scratch path may appear in the command: {cmd}"
        );
        assert!(!cmd.contains("tmp"), "no scratch file at all: {cmd}");
    }

    /// A symlinked rc file falls back to append-only, exactly matching
    /// `append_if_absent_command`'s behaviour. Truncating and rewriting
    /// through a symlink as root would be strictly worse than what we had.
    ///
    /// The target is seeded with a *stale block* so the assertion can tell the
    /// guard fired: with it, the strip is skipped and the stale block survives
    /// beside the new one; without it, the stale block would be rewritten away.
    #[test]
    fn injection_does_not_rewrite_through_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("real-file");
        std::fs::write(
            &target,
            "important\n# >>> cella shell completion v0 >>>\nstale\n\
             # <<< cella shell completion <<<\n",
        )
        .unwrap();
        let rc = dir.path().join(".bashrc");
        std::os::unix::fs::symlink(&target, &rc).unwrap();
        let block = &COMPLETION_BLOCKS[0];

        run_injection(&rc, block);

        let after = std::fs::read_to_string(&target).unwrap();
        assert!(after.starts_with("important\n"), "{after}");
        assert!(
            after.contains("stale"),
            "a symlinked rc must not be rewritten, only appended to: {after}"
        );
        assert_eq!(
            blocks_in(&target),
            2,
            "append-only means both blocks: {after}"
        );
        assert!(
            std::fs::symlink_metadata(&rc).unwrap().is_symlink(),
            "the symlink itself must survive"
        );
    }

    /// Absent rc file: nothing is created,    /// Absent rc file: nothing is created, nothing is printed, exit 0. Such a
    /// container gets no PATH block either, so `cella` is not on PATH there.
    #[test]
    fn injection_skips_a_missing_profile() {
        let dir = tempfile::tempdir().unwrap();
        let rc = dir.path().join(".zshrc");
        let block = &COMPLETION_BLOCKS[0];

        run_injection(&rc, block);

        assert!(!rc.exists(), "must not create the profile");
    }

    /// Fallback on an image with no usable `awk`: the strip cannot run, the
    /// append still does, and the result degrades to exactly the additive
    /// behaviour `append_if_absent_command` already has — the stale block survives
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
        let block = &COMPLETION_BLOCKS[0];

        let cmd = managed_block_command(&rc.to_string_lossy(), block);
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
        let block = &COMPLETION_BLOCKS[0];

        run_injection(&rc, block);

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
            .map(|(g, _)| *g)
            .chain(COMPLETION_BLOCKS.iter().map(|b| b.guard))
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
