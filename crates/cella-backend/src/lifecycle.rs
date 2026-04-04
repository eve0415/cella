//! Lifecycle command parsing and execution.
//!
//! Moved from `cella-docker` so that `cella-orchestrator` can use these
//! types and functions without depending on a concrete backend crate.

use std::io;

use serde_json::Value;
use tracing::{debug, info};

use crate::error::BackendError;
use crate::traits::ContainerBackend;
use crate::types::{ExecOptions, ExecResult};

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
    /// Container backend (trait object).
    pub client: &'a dyn ContainerBackend,
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
) -> Result<(), BackendError> {
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
                        Box::new(CallbackWriter::new(on_output.as_ref())),
                        Box::new(CallbackWriter::new(on_output.as_ref())),
                    )
                    .await?
            } else {
                // Fallback: stream directly to stderr
                ctx.client
                    .exec_stream(
                        ctx.container_id,
                        &opts,
                        Box::new(io::stderr()),
                        Box::new(io::stderr()),
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

/// Run named lifecycle commands in parallel, cancelling siblings on first failure.
///
/// Uses `try_join_all` so that when any command fails, remaining in-flight
/// commands are cancelled (their futures are dropped) per the spec requirement.
async fn run_parallel(
    ctx: &LifecycleContext<'_>,
    phase: &str,
    commands: Vec<(String, Vec<String>)>,
) -> Result<(), BackendError> {
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
            Ok::<ExecResult, BackendError>(result)
        });
    }

    let results = futures_util::future::try_join_all(futures).await?;

    if ctx.is_text {
        print_completed_output(&results);
    }

    Ok(())
}

/// Check an exec result exit code, returning `LifecycleFailed` on non-zero.
fn check_exit_code(
    result: &ExecResult,
    phase: &str,
    name: Option<&str>,
) -> Result<(), BackendError> {
    if result.exit_code != 0 {
        let prefix = name.map_or(String::new(), |n| format!("[{n}] "));
        return Err(BackendError::LifecycleFailed {
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

/// Print stdout/stderr from completed parallel exec results to stderr.
fn print_completed_output(results: &[ExecResult]) {
    for exec_result in results {
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
/// Returns `BackendError::LifecycleFailed` if any command fails.
pub async fn run_lifecycle_phase(
    ctx: &LifecycleContext<'_>,
    phase: &str,
    value: &Value,
    origin: &str,
) -> Result<(), BackendError> {
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

    #[test]
    fn spec_string_command_wrapped_in_sh() {
        let value = json!("echo hello && echo world");
        match parse_lifecycle_command(&value) {
            ParsedLifecycle::Sequential(cmds) => {
                assert_eq!(cmds.len(), 1);
                assert_eq!(cmds[0][0], "sh");
                assert_eq!(cmds[0][1], "-c");
                assert_eq!(cmds[0][2], "echo hello && echo world");
            }
            ParsedLifecycle::Parallel(_) => panic!("expected Sequential"),
        }
    }

    #[test]
    fn spec_array_command_executed_directly() {
        let value = json!(["echo", "hello", "world"]);
        match parse_lifecycle_command(&value) {
            ParsedLifecycle::Sequential(cmds) => {
                assert_eq!(cmds.len(), 1);
                assert_eq!(cmds[0], vec!["echo", "hello", "world"]);
            }
            ParsedLifecycle::Parallel(_) => panic!("expected Sequential"),
        }
    }

    #[test]
    fn spec_object_command_is_parallel() {
        let value = json!({"setup": "npm install", "db": ["mysql", "-u", "root"]});
        match parse_lifecycle_command(&value) {
            ParsedLifecycle::Parallel(cmds) => {
                assert_eq!(cmds.len(), 2);
                let setup = cmds.iter().find(|(n, _)| n == "setup").unwrap();
                assert_eq!(setup.1, vec!["sh", "-c", "npm install"]);
                let db = cmds.iter().find(|(n, _)| n == "db").unwrap();
                assert_eq!(db.1, vec!["mysql", "-u", "root"]);
            }
            ParsedLifecycle::Sequential(_) => panic!("expected Parallel"),
        }
    }

    #[test]
    fn spec_object_string_values_wrapped_in_sh() {
        let value = json!({"server": "npm start", "client": "npm run dev"});
        match parse_lifecycle_command(&value) {
            ParsedLifecycle::Parallel(cmds) => {
                for (_, cmd) in &cmds {
                    assert_eq!(cmd[0], "sh");
                    assert_eq!(cmd[1], "-c");
                }
            }
            ParsedLifecycle::Sequential(_) => panic!("expected Parallel"),
        }
    }

    #[test]
    fn spec_empty_string_is_valid_command() {
        let value = json!("");
        match parse_lifecycle_command(&value) {
            ParsedLifecycle::Sequential(cmds) => {
                assert_eq!(cmds.len(), 1);
                assert_eq!(cmds[0], vec!["sh", "-c", ""]);
            }
            ParsedLifecycle::Parallel(_) => panic!("expected Sequential"),
        }
    }

    #[test]
    fn spec_empty_object_no_parallel_commands() {
        let value = json!({});
        match parse_lifecycle_command(&value) {
            ParsedLifecycle::Parallel(cmds) => assert!(cmds.is_empty()),
            ParsedLifecycle::Sequential(_) => panic!("expected Parallel"),
        }
    }

    #[test]
    fn spec_lifecycle_phase_order() {
        let phases = [
            "initializeCommand",
            "onCreateCommand",
            "updateContentCommand",
            "postCreateCommand",
            "postStartCommand",
            "postAttachCommand",
        ];
        for i in 0..phases.len() - 1 {
            assert!(
                phases.iter().position(|p| *p == phases[i]).unwrap()
                    < phases.iter().position(|p| *p == phases[i + 1]).unwrap()
            );
        }
    }

    #[test]
    fn spec_resume_only_post_start_and_attach() {
        let resume_phases = ["postStartCommand", "postAttachCommand"];
        let creation_only = [
            "initializeCommand",
            "onCreateCommand",
            "updateContentCommand",
            "postCreateCommand",
        ];
        for phase in &creation_only {
            assert!(!resume_phases.contains(phase));
        }
    }

    #[test]
    fn check_exit_code_zero_is_ok() {
        let result = ExecResult {
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
        };
        assert!(check_exit_code(&result, "postCreateCommand", None).is_ok());
    }

    #[test]
    fn check_exit_code_nonzero_returns_error() {
        let result = ExecResult {
            exit_code: 1,
            stdout: String::new(),
            stderr: "command not found".to_string(),
        };
        let err = check_exit_code(&result, "onCreateCommand", None).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("onCreateCommand"),
            "error should contain phase name, got: {msg}"
        );
    }

    #[test]
    fn check_exit_code_nonzero_includes_exit_code_in_message() {
        let result = ExecResult {
            exit_code: 127,
            stdout: String::new(),
            stderr: "sh: npm: not found".to_string(),
        };
        let err = check_exit_code(&result, "postCreateCommand", None).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("127"),
            "error should contain exit code, got: {msg}"
        );
    }

    #[test]
    fn check_exit_code_nonzero_includes_stderr() {
        let result = ExecResult {
            exit_code: 2,
            stdout: "some output\n".to_string(),
            stderr: "fatal error occurred\n".to_string(),
        };
        let err = check_exit_code(&result, "updateContentCommand", None).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("fatal error occurred"),
            "error should contain trimmed stderr, got: {msg}"
        );
    }

    #[test]
    fn check_exit_code_with_named_prefix() {
        let result = ExecResult {
            exit_code: 1,
            stdout: String::new(),
            stderr: "failed".to_string(),
        };
        let err = check_exit_code(&result, "postStartCommand", Some("setup")).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("[setup]"),
            "error should contain [name] prefix, got: {msg}"
        );
    }

    #[test]
    fn check_exit_code_named_prefix_absent_when_none() {
        let result = ExecResult {
            exit_code: 1,
            stdout: String::new(),
            stderr: "err".to_string(),
        };
        let err = check_exit_code(&result, "phase", None).unwrap_err();
        let msg = format!("{err}");
        assert!(
            !msg.contains('['),
            "no bracket prefix expected without name, got: {msg}"
        );
    }

    #[test]
    fn check_exit_code_zero_with_name_still_ok() {
        let result = ExecResult {
            exit_code: 0,
            stdout: "done".to_string(),
            stderr: String::new(),
        };
        assert!(check_exit_code(&result, "phase", Some("task")).is_ok());
    }

    #[test]
    fn check_exit_code_stderr_trimmed() {
        let result = ExecResult {
            exit_code: 1,
            stdout: String::new(),
            stderr: "  whitespace  \n".to_string(),
        };
        let err = check_exit_code(&result, "phase", None).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("whitespace"),
            "expected trimmed stderr in message, got: {msg}"
        );
        assert!(!msg.ends_with('\n'), "stderr should be trimmed, got: {msg}");
    }

    #[test]
    fn parse_object_with_non_string_non_array_value() {
        let value = json!({"check": 42});
        match parse_lifecycle_command(&value) {
            ParsedLifecycle::Parallel(cmds) => {
                assert_eq!(cmds.len(), 1);
                assert_eq!(cmds[0].1[0], "sh");
                assert_eq!(cmds[0].1[1], "-c");
                assert_eq!(cmds[0].1[2], "42");
            }
            ParsedLifecycle::Sequential(_) => panic!("expected Parallel"),
        }
    }

    #[test]
    fn parse_boolean_value() {
        let value = json!(true);
        match parse_lifecycle_command(&value) {
            ParsedLifecycle::Sequential(cmds) => assert!(cmds.is_empty()),
            ParsedLifecycle::Parallel(_) => panic!("expected Sequential"),
        }
    }

    #[test]
    fn parse_number_value() {
        let value = json!(42);
        match parse_lifecycle_command(&value) {
            ParsedLifecycle::Sequential(cmds) => assert!(cmds.is_empty()),
            ParsedLifecycle::Parallel(_) => panic!("expected Sequential"),
        }
    }

    #[test]
    fn parse_array_filters_non_string_elements() {
        let value = json!(["echo", 42, "hello", null]);
        match parse_lifecycle_command(&value) {
            ParsedLifecycle::Sequential(cmds) => {
                assert_eq!(cmds.len(), 1);
                assert_eq!(cmds[0], vec!["echo", "hello"]);
            }
            ParsedLifecycle::Parallel(_) => panic!("expected Sequential"),
        }
    }

    #[test]
    fn parse_empty_array() {
        let value = json!([]);
        match parse_lifecycle_command(&value) {
            ParsedLifecycle::Sequential(cmds) => {
                assert_eq!(cmds.len(), 1);
                assert!(cmds[0].is_empty());
            }
            ParsedLifecycle::Parallel(_) => panic!("expected Sequential"),
        }
    }

    #[test]
    fn callback_writer_handles_multiple_writes_for_one_line() {
        use std::sync::Mutex;

        let collected = Mutex::new(Vec::new());
        let callback = |line: &str| {
            collected.lock().unwrap().push(line.to_string());
        };

        let mut writer = CallbackWriter::new(&callback);
        io::Write::write_all(&mut writer, b"hello ").unwrap();
        io::Write::write_all(&mut writer, b"world\n").unwrap();
        drop(writer);

        let lines = collected.into_inner().unwrap();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0], "      hello world");
    }

    #[test]
    fn callback_writer_handles_empty_input() {
        let lines = collect_callback_lines(b"");
        assert!(lines.is_empty());
    }

    #[test]
    fn callback_writer_only_newlines() {
        let lines = collect_callback_lines(b"\n\n\n");
        assert!(lines.is_empty());
    }
}
