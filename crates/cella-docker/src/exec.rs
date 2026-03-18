//! Execute commands inside a running container.

use bollard::exec::{CreateExecOptions, StartExecResults};
use futures_util::StreamExt;
use tracing::debug;

use crate::CellaDockerError;
use crate::client::DockerClient;

/// Options for executing a command in a container.
pub struct ExecOptions {
    pub cmd: Vec<String>,
    pub user: Option<String>,
    pub env: Option<Vec<String>>,
    pub working_dir: Option<String>,
}

/// Result of a command execution.
pub struct ExecResult {
    pub exit_code: i64,
    pub stdout: String,
    pub stderr: String,
}

impl DockerClient {
    /// Execute a command inside a running container.
    ///
    /// # Errors
    ///
    /// Returns `CellaDockerError::DockerApi` on API errors.
    pub async fn exec_command(
        &self,
        container_id: &str,
        opts: &ExecOptions,
    ) -> Result<ExecResult, CellaDockerError> {
        let cmd: Vec<&str> = opts.cmd.iter().map(String::as_str).collect();
        debug!("Exec in {container_id}: {}", opts.cmd.join(" "));

        let env_refs: Option<Vec<&str>> = opts
            .env
            .as_ref()
            .map(|e| e.iter().map(String::as_str).collect());

        let create_opts = CreateExecOptions {
            cmd: Some(cmd),
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            user: opts.user.as_deref(),
            working_dir: opts.working_dir.as_deref(),
            env: env_refs,
            ..Default::default()
        };

        let exec = self.inner().create_exec(container_id, create_opts).await?;

        let mut stdout = String::new();
        let mut stderr = String::new();

        let start_result = self.inner().start_exec(&exec.id, None).await?;

        if let StartExecResults::Attached { mut output, .. } = start_result {
            while let Some(chunk) = output.next().await {
                match chunk? {
                    bollard::container::LogOutput::StdOut { message } => {
                        stdout.push_str(&String::from_utf8_lossy(&message));
                    }
                    bollard::container::LogOutput::StdErr { message } => {
                        stderr.push_str(&String::from_utf8_lossy(&message));
                    }
                    _ => {}
                }
            }
        }

        let inspect = self.inner().inspect_exec(&exec.id).await?;
        let exit_code = inspect.exit_code.unwrap_or(0);

        debug!("Exec exit code: {exit_code}");

        Ok(ExecResult {
            exit_code,
            stdout,
            stderr,
        })
    }
}
