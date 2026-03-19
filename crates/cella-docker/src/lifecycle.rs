//! Lifecycle command parsing and execution.

use serde_json::Value;
use tracing::debug;

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

/// Execute lifecycle commands for a phase.
///
/// When `is_text` is true, prints origin-tracked progress (matching the original
/// devcontainer CLI phrasing) and streams sequential command output to stderr.
///
/// # Errors
///
/// Returns `CellaDockerError::LifecycleFailed` if any command fails.
#[allow(clippy::too_many_arguments)]
pub async fn run_lifecycle_phase(
    client: &DockerClient,
    container_id: &str,
    phase: &str,
    value: &Value,
    origin: &str,
    user: Option<&str>,
    env: &[String],
    working_dir: Option<&str>,
    is_text: bool,
) -> Result<(), CellaDockerError> {
    if is_text {
        eprintln!("Running the {phase} from {origin}...");
    }
    debug!("Running {phase} from {origin}");

    let parsed = parse_lifecycle_command(value);

    match parsed {
        ParsedLifecycle::Sequential(commands) => {
            for cmd in commands {
                if cmd.is_empty() {
                    continue;
                }
                debug!("{phase}: {}", cmd.join(" "));
                let opts = ExecOptions {
                    cmd,
                    user: user.map(String::from),
                    env: Some(env.to_vec()),
                    working_dir: working_dir.map(String::from),
                };
                let result = if is_text {
                    client
                        .exec_stream(container_id, &opts, std::io::stderr(), std::io::stderr())
                        .await?
                } else {
                    client.exec_command(container_id, &opts).await?
                };

                if result.exit_code != 0 {
                    return Err(CellaDockerError::LifecycleFailed {
                        phase: phase.to_string(),
                        message: format!(
                            "exit code {}: {}",
                            result.exit_code,
                            result.stderr.trim()
                        ),
                    });
                }
            }
        }
        ParsedLifecycle::Parallel(commands) => {
            let mut futures = Vec::new();
            for (name, cmd) in commands {
                let user = user.map(String::from);
                let env = env.to_vec();
                let working_dir = working_dir.map(String::from);
                let phase = phase.to_string();
                let container_id = container_id.to_string();

                futures.push(async move {
                    debug!("{phase} [{name}]: {}", cmd.join(" "));
                    let result = client
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

                    if result.exit_code != 0 {
                        return Err(CellaDockerError::LifecycleFailed {
                            phase,
                            message: format!(
                                "[{name}] exit code {}: {}",
                                result.exit_code,
                                result.stderr.trim()
                            ),
                        });
                    }
                    Ok::<ExecResult, CellaDockerError>(result)
                });
            }

            let results = futures_util::future::join_all(futures).await;

            if is_text {
                for exec_result in results.iter().flatten() {
                    if !exec_result.stdout.is_empty() {
                        eprint!("{}", exec_result.stdout);
                    }
                    if !exec_result.stderr.is_empty() {
                        eprint!("{}", exec_result.stderr);
                    }
                }
            }

            for result in results {
                let _ = result?;
            }
        }
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
}
