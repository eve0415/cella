//! Lifecycle command parsing and execution.

use std::io;

use serde_json::Value;
use tracing::{debug, info};

use crate::CellaDockerError;
use crate::client::DockerClient;
use crate::exec::{ExecOptions, ExecResult};

/// Parsed lifecycle command.
pub enum ParsedLifecycle {
    /// Sequential commands.
    Sequential(Vec<Vec<String>>),
    /// Named commands to run in parallel.
    Parallel(Vec<(String, Vec<String>)>),
}

/// Parse a lifecycle command value into executable commands.
///
/// Handles: string → shell command, array → direct command, object → parallel named commands.
pub fn parse_lifecycle_command(value: &Value) -> ParsedLifecycle {
    match value {
        Value::String(s) => {
            ParsedLifecycle::Sequential(vec![vec!["sh".to_string(), "-c".to_string(), s.clone()]])
        }
        Value::Array(arr) => {
            let cmd: Vec<String> = arr
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect();
            ParsedLifecycle::Sequential(vec![cmd])
        }
        Value::Object(map) => {
            let commands: Vec<(String, Vec<String>)> = map
                .iter()
                .map(|(name, v)| {
                    let cmd = match v {
                        Value::String(s) => {
                            vec!["sh".to_string(), "-c".to_string(), s.clone()]
                        }
                        Value::Array(arr) => arr
                            .iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect(),
                        _ => vec!["sh".to_string(), "-c".to_string(), v.to_string()],
                    };
                    (name.clone(), cmd)
                })
                .collect();
            ParsedLifecycle::Parallel(commands)
        }
        _ => ParsedLifecycle::Sequential(vec![]),
    }
}

/// Callback type for routing lifecycle output through a progress system.
pub type OutputCallback<'a> = Box<dyn Fn(&str) + Send + Sync + 'a>;

/// Shared container context for lifecycle phase execution.
pub struct LifecycleContext<'a> {
    /// Docker client.
    pub client: &'a DockerClient,
    /// Container to run commands in.
    pub container_id: &'a str,
    /// User to run commands as.
    pub user: Option<&'a str>,
    /// Environment variables.
    pub env: &'a [String],
    /// Working directory inside the container.
    pub working_dir: Option<&'a str>,
    /// Whether to print progress and stream output to stderr.
    pub is_text: bool,
    /// Optional callback for routing output lines through a progress system.
    ///
    /// When set, sequential lifecycle output is written through this callback
    /// (e.g., indented under an active spinner) instead of directly to stderr.
    pub on_output: Option<OutputCallback<'a>>,
}

/// A `Write` adapter that buffers lines and forwards each complete line
/// through a callback with indentation.
struct CallbackWriter<'a> {
    callback: &'a (dyn Fn(&str) + Send + Sync),
    buf: Vec<u8>,
}

impl<'a> CallbackWriter<'a> {
    fn new(callback: &'a (dyn Fn(&str) + Send + Sync)) -> Self {
        Self {
            callback,
            buf: Vec::with_capacity(256),
        }
    }

    fn flush_lines(&mut self) {
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let line = String::from_utf8_lossy(&self.buf[..pos]);
            if !line.trim().is_empty() {
                (self.callback)(&format!("      {line}"));
            }
            self.buf.drain(..=pos);
        }
    }

    fn flush_remaining(&mut self) {
        if !self.buf.is_empty() {
            let line = String::from_utf8_lossy(&self.buf);
            if !line.trim().is_empty() {
                (self.callback)(&format!("      {line}"));
            }
            self.buf.clear();
        }
    }
}

impl io::Write for CallbackWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(buf);
        self.flush_lines();
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flush_remaining();
        Ok(())
    }
}

impl Drop for CallbackWriter<'_> {
    fn drop(&mut self) {
        self.flush_remaining();
    }
}

/// Run sequential lifecycle commands, streaming output when `is_text`.
async fn run_sequential(
    ctx: &LifecycleContext<'_>,
    phase: &str,
    commands: Vec<Vec<String>>,
) -> Result<(), CellaDockerError> {
    for cmd in commands {
        if cmd.is_empty() {
            continue;
        }
        debug!("{phase}: {}", cmd.join(" "));
        let opts = ExecOptions {
            cmd,
            user: ctx.user.map(String::from),
            env: Some(ctx.env.to_vec()),
            working_dir: ctx.working_dir.map(String::from),
        };
        let result = if ctx.is_text {
            if let Some(ref on_output) = ctx.on_output {
                // Route through progress system with indentation
                ctx.client
                    .exec_stream(
                        ctx.container_id,
                        &opts,
                        CallbackWriter::new(on_output.as_ref()),
                        CallbackWriter::new(on_output.as_ref()),
                    )
                    .await?
            } else {
                // Fallback: stream directly to stderr
                ctx.client
                    .exec_stream(
                        ctx.container_id,
                        &opts,
                        std::io::stderr(),
                        std::io::stderr(),
                    )
                    .await?
            }
        } else {
            ctx.client.exec_command(ctx.container_id, &opts).await?
        };

        check_exit_code(&result, phase, None)?;
    }
    Ok(())
}

/// Run named lifecycle commands in parallel, collecting and printing output.
async fn run_parallel(
    ctx: &LifecycleContext<'_>,
    phase: &str,
    commands: Vec<(String, Vec<String>)>,
) -> Result<(), CellaDockerError> {
    let mut futures = Vec::new();
    for (name, cmd) in commands {
        let user = ctx.user.map(String::from);
        let env = ctx.env.to_vec();
        let working_dir = ctx.working_dir.map(String::from);
        let phase = phase.to_string();
        let container_id = ctx.container_id.to_string();

        futures.push(async move {
            debug!("{phase} [{name}]: {}", cmd.join(" "));
            let result = ctx
                .client
                .exec_command(
                    &container_id,
                    &ExecOptions {
                        cmd,
                        user,
                        env: Some(env),
                        working_dir,
                    },
                )
                .await?;

            check_exit_code(&result, &phase, Some(&name))?;
            Ok::<ExecResult, CellaDockerError>(result)
        });
    }

    let results = futures_util::future::join_all(futures).await;

    if ctx.is_text {
        print_parallel_output(&results);
    }

    for result in results {
        let _ = result?;
    }
    Ok(())
}

/// Check an exec result exit code, returning `LifecycleFailed` on non-zero.
fn check_exit_code(
    result: &ExecResult,
    phase: &str,
    name: Option<&str>,
) -> Result<(), CellaDockerError> {
    if result.exit_code != 0 {
        let prefix = name.map_or(String::new(), |n| format!("[{n}] "));
        return Err(CellaDockerError::LifecycleFailed {
            phase: phase.to_string(),
            message: format!(
                "{prefix}exit code {}: {}",
                result.exit_code,
                result.stderr.trim()
            ),
        });
    }
    Ok(())
}

/// Print stdout/stderr from parallel exec results to stderr.
fn print_parallel_output(results: &[Result<ExecResult, CellaDockerError>]) {
    for exec_result in results.iter().flatten() {
        if !exec_result.stdout.is_empty() {
            eprint!("{}", exec_result.stdout);
        }
        if !exec_result.stderr.is_empty() {
            eprint!("{}", exec_result.stderr);
        }
    }
}

/// Execute lifecycle commands for a phase.
///
/// When `ctx.is_text` is true, prints origin-tracked progress (matching the original
/// devcontainer CLI phrasing) and streams sequential command output to stderr.
///
/// # Errors
///
/// Returns `CellaDockerError::LifecycleFailed` if any command fails.
pub async fn run_lifecycle_phase(
    ctx: &LifecycleContext<'_>,
    phase: &str,
    value: &Value,
    origin: &str,
) -> Result<(), CellaDockerError> {
    info!("Running the {phase} from {origin}...");
    debug!("Running {phase} from {origin}");

    match parse_lifecycle_command(value) {
        ParsedLifecycle::Sequential(commands) => run_sequential(ctx, phase, commands).await?,
        ParsedLifecycle::Parallel(commands) => run_parallel(ctx, phase, commands).await?,
    }

    debug!("{phase} completed");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_string_command() {
        let value = json!("echo hello");
        match parse_lifecycle_command(&value) {
            ParsedLifecycle::Sequential(cmds) => {
                assert_eq!(cmds.len(), 1);
                assert_eq!(cmds[0], vec!["sh", "-c", "echo hello"]);
            }
            ParsedLifecycle::Parallel(_) => panic!("expected Sequential"),
        }
    }

    #[test]
    fn parse_array_command() {
        let value = json!(["echo", "hello"]);
        match parse_lifecycle_command(&value) {
            ParsedLifecycle::Sequential(cmds) => {
                assert_eq!(cmds.len(), 1);
                assert_eq!(cmds[0], vec!["echo", "hello"]);
            }
            ParsedLifecycle::Parallel(_) => panic!("expected Sequential"),
        }
    }

    #[test]
    fn parse_object_commands() {
        let value = json!({"setup": "echo setup", "install": ["npm", "i"]});
        match parse_lifecycle_command(&value) {
            ParsedLifecycle::Parallel(cmds) => {
                assert_eq!(cmds.len(), 2);
            }
            ParsedLifecycle::Sequential(_) => panic!("expected Parallel"),
        }
    }

    #[test]
    fn parse_null_value() {
        let value = json!(null);
        match parse_lifecycle_command(&value) {
            ParsedLifecycle::Sequential(cmds) => {
                assert!(cmds.is_empty());
            }
            ParsedLifecycle::Parallel(_) => panic!("expected Sequential"),
        }
    }

    fn collect_callback_lines(input: &[u8]) -> Vec<String> {
        use std::sync::Mutex;

        let collected = Mutex::new(Vec::new());
        let callback = |line: &str| {
            collected.lock().unwrap().push(line.to_string());
        };

        let mut writer = CallbackWriter::new(&callback);
        io::Write::write_all(&mut writer, input).unwrap();
        drop(writer);

        collected.into_inner().unwrap()
    }

    #[test]
    fn callback_writer_indents_lines() {
        let lines = collect_callback_lines(b"first line\nsecond line\n");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "      first line");
        assert_eq!(lines[1], "      second line");
    }

    #[test]
    fn callback_writer_flushes_partial_line_on_drop() {
        let lines = collect_callback_lines(b"no newline");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0], "      no newline");
    }

    #[test]
    fn callback_writer_skips_blank_lines() {
        let lines = collect_callback_lines(b"content\n\n  \nanother\n");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "      content");
        assert_eq!(lines[1], "      another");
    }
}
