//! Execute commands inside a running container.

use bollard::exec::{CreateExecOptions, ResizeExecOptions, StartExecOptions, StartExecResults};
use futures_util::StreamExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::debug;

use crate::CellaDockerError;
use crate::client::DockerClient;

/// Options for executing a command in a container (capture mode).
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

/// Options for interactive command execution.
pub struct InteractiveExecOptions {
    pub cmd: Vec<String>,
    pub user: Option<String>,
    pub env: Option<Vec<String>>,
    pub working_dir: Option<String>,
    pub tty: bool,
}

/// Guard that restores terminal raw mode on drop.
struct RawModeGuard;

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

impl DockerClient {
    /// Execute a command inside a running container (captures output).
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

    /// Execute a command interactively with stdin/stdout/stderr forwarding.
    ///
    /// When `tty` is true, enables raw mode and forwards terminal size changes.
    /// Returns the exit code of the command.
    ///
    /// # Errors
    ///
    /// Returns `CellaDockerError::DockerApi` on API errors, or
    /// `CellaDockerError::Io` on I/O errors.
    #[allow(clippy::too_many_lines)]
    pub async fn exec_interactive(
        &self,
        container_id: &str,
        opts: &InteractiveExecOptions,
    ) -> Result<i64, CellaDockerError> {
        let cmd: Vec<&str> = opts.cmd.iter().map(String::as_str).collect();
        debug!("Interactive exec in {container_id}: {}", opts.cmd.join(" "));

        let env_refs: Option<Vec<&str>> = opts
            .env
            .as_ref()
            .map(|e| e.iter().map(String::as_str).collect());

        let create_opts = CreateExecOptions {
            cmd: Some(cmd),
            attach_stdin: Some(true),
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            tty: Some(opts.tty),
            user: opts.user.as_deref(),
            working_dir: opts.working_dir.as_deref(),
            env: env_refs,
            ..Default::default()
        };

        let exec = self.inner().create_exec(container_id, create_opts).await?;

        let start_opts = StartExecOptions {
            detach: false,
            ..Default::default()
        };

        let start_result = self.inner().start_exec(&exec.id, Some(start_opts)).await?;

        let StartExecResults::Attached { mut output, input } = start_result else {
            return Ok(0);
        };

        // Enable raw mode for TTY sessions
        let raw_guard = if opts.tty {
            crossterm::terminal::enable_raw_mode()?;
            // Send initial terminal size
            if let Ok((cols, rows)) = crossterm::terminal::size() {
                let _ = self
                    .inner()
                    .resize_exec(
                        &exec.id,
                        ResizeExecOptions {
                            width: cols,
                            height: rows,
                        },
                    )
                    .await;
            }
            Some(RawModeGuard)
        } else {
            None
        };

        let exec_id = exec.id.clone();
        let docker = self.inner().clone();

        // Stdin → container input
        let stdin_handle = tokio::spawn(async move {
            let mut stdin = tokio::io::stdin();
            let mut input = input;
            let mut buf = [0u8; 1024];
            loop {
                match stdin.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if input.write_all(&buf[..n]).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });

        // Container output → stdout
        let output_handle = tokio::spawn(async move {
            let mut stdout = tokio::io::stdout();
            while let Some(chunk) = output.next().await {
                match chunk {
                    Ok(
                        bollard::container::LogOutput::StdOut { message }
                        | bollard::container::LogOutput::StdErr { message }
                        | bollard::container::LogOutput::Console { message },
                    ) => {
                        if stdout.write_all(&message).await.is_err() {
                            break;
                        }
                        let _ = stdout.flush().await;
                    }
                    Err(_) | Ok(_) => break,
                }
            }
        });

        // SIGWINCH handler for terminal resize (unix only)
        #[cfg(unix)]
        let resize_handle = if opts.tty {
            let exec_id_resize = exec_id.clone();
            let docker_resize = docker.clone();
            Some(tokio::spawn(async move {
                let Ok(mut sig) =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())
                else {
                    return;
                };
                while sig.recv().await.is_some() {
                    if let Ok((cols, rows)) = crossterm::terminal::size() {
                        let _ = docker_resize
                            .resize_exec(
                                &exec_id_resize,
                                ResizeExecOptions {
                                    width: cols,
                                    height: rows,
                                },
                            )
                            .await;
                    }
                }
            }))
        } else {
            None
        };

        // Wait for output to finish (command exit)
        let _ = output_handle.await;
        stdin_handle.abort();

        #[cfg(unix)]
        if let Some(h) = resize_handle {
            h.abort();
        }

        // Drop raw mode guard before inspecting
        drop(raw_guard);

        let inspect = self.inner().inspect_exec(&exec_id).await?;
        Ok(inspect.exit_code.unwrap_or(0))
    }

    /// Execute a command in detached mode.
    ///
    /// Returns the exec instance ID.
    ///
    /// # Errors
    ///
    /// Returns `CellaDockerError::DockerApi` on API errors.
    pub async fn exec_detached(
        &self,
        container_id: &str,
        opts: &ExecOptions,
    ) -> Result<String, CellaDockerError> {
        let cmd: Vec<&str> = opts.cmd.iter().map(String::as_str).collect();
        debug!("Detached exec in {container_id}: {}", opts.cmd.join(" "));

        let env_refs: Option<Vec<&str>> = opts
            .env
            .as_ref()
            .map(|e| e.iter().map(String::as_str).collect());

        let create_opts = CreateExecOptions {
            cmd: Some(cmd),
            user: opts.user.as_deref(),
            working_dir: opts.working_dir.as_deref(),
            env: env_refs,
            ..Default::default()
        };

        let exec = self.inner().create_exec(container_id, create_opts).await?;

        let start_opts = StartExecOptions {
            detach: true,
            ..Default::default()
        };

        self.inner().start_exec(&exec.id, Some(start_opts)).await?;

        Ok(exec.id)
    }
}
