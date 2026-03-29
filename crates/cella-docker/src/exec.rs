//! Execute commands inside a running container.

use std::io::Write;
use std::pin::Pin;

use bollard::Docker;
use bollard::container::LogOutput;
use bollard::exec::{CreateExecOptions, ResizeExecOptions, StartExecOptions, StartExecResults};
use futures_util::{Stream, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::task::JoinHandle;
use tracing::debug;

pub use cella_backend::{ExecOptions, ExecResult, InteractiveExecOptions};

use crate::CellaDockerError;
use crate::client::DockerClient;

/// Pinned stream of container log output chunks.
type OutputStream = Pin<Box<dyn Stream<Item = Result<LogOutput, bollard::errors::Error>> + Send>>;

/// Pinned async writer for container stdin.
type InputStream = Pin<Box<dyn AsyncWrite + Send>>;

/// Guard that restores terminal raw mode on drop.
struct RawModeGuard;

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

/// Build base `CreateExecOptions` from `ExecOptions`, collecting owned reference vectors.
fn build_base_exec_options(opts: &ExecOptions) -> CreateExecOptions<&str> {
    let cmd: Vec<&str> = opts.cmd.iter().map(String::as_str).collect();
    let env_refs: Option<Vec<&str>> = opts
        .env
        .as_ref()
        .map(|e| e.iter().map(String::as_str).collect());

    CreateExecOptions {
        cmd: Some(cmd),
        user: opts.user.as_deref(),
        working_dir: opts.working_dir.as_deref(),
        env: env_refs,
        ..Default::default()
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
        debug!("Exec in {container_id}: {}", opts.cmd.join(" "));

        let create_opts = CreateExecOptions {
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            ..build_base_exec_options(opts)
        };

        let exec = self.inner().create_exec(container_id, create_opts).await?;

        let mut stdout = String::new();
        let mut stderr = String::new();

        let start_result = self.inner().start_exec(&exec.id, None).await?;

        if let StartExecResults::Attached { mut output, .. } = start_result {
            while let Some(chunk) = output.next().await {
                match chunk? {
                    LogOutput::StdOut { message } => {
                        stdout.push_str(&String::from_utf8_lossy(&message));
                    }
                    LogOutput::StdErr { message } => {
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

    /// Execute a command inside a running container (streams output).
    ///
    /// Like [`exec_command`](Self::exec_command), but writes output chunks to the
    /// provided writers as they arrive.  Still accumulates full output strings in
    /// the returned [`ExecResult`] for programmatic inspection.
    ///
    /// # Errors
    ///
    /// Returns `CellaDockerError::DockerApi` on API errors.
    pub async fn exec_stream(
        &self,
        container_id: &str,
        opts: &ExecOptions,
        mut stdout_writer: impl Write + Send,
        mut stderr_writer: impl Write + Send,
    ) -> Result<ExecResult, CellaDockerError> {
        debug!("Exec stream in {container_id}: {}", opts.cmd.join(" "));

        let create_opts = CreateExecOptions {
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            ..build_base_exec_options(opts)
        };

        let exec = self.inner().create_exec(container_id, create_opts).await?;

        let mut stdout = String::new();
        let mut stderr = String::new();

        let start_result = self.inner().start_exec(&exec.id, None).await?;

        if let StartExecResults::Attached { mut output, .. } = start_result {
            while let Some(chunk) = output.next().await {
                match chunk? {
                    LogOutput::StdOut { message } => {
                        stdout.push_str(&String::from_utf8_lossy(&message));
                        let _ = stdout_writer.write_all(&message);
                        let _ = stdout_writer.flush();
                    }
                    LogOutput::StdErr { message } => {
                        stderr.push_str(&String::from_utf8_lossy(&message));
                        let _ = stderr_writer.write_all(&message);
                        let _ = stderr_writer.flush();
                    }
                    _ => {}
                }
            }
        }

        let inspect = self.inner().inspect_exec(&exec.id).await?;
        let exit_code = inspect.exit_code.unwrap_or(0);

        debug!("Exec stream exit code: {exit_code}");

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

        let Some((exec_id, output, input)) =
            start_attached_exec(self.inner(), container_id, create_opts).await?
        else {
            return Ok(0);
        };

        let raw_guard = enable_tty_raw_mode(self.inner(), &exec_id, opts.tty).await?;

        let docker = self.inner().clone();
        let (stdin_handle, output_handle) = spawn_io_tasks(input, output);

        #[cfg(unix)]
        let resize_handle = spawn_resize_handler(&exec_id, &docker, opts.tty);

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
        debug!("Detached exec in {container_id}: {}", opts.cmd.join(" "));

        let create_opts = build_base_exec_options(opts);

        let exec = self.inner().create_exec(container_id, create_opts).await?;

        let start_opts = StartExecOptions {
            detach: true,
            ..Default::default()
        };

        self.inner().start_exec(&exec.id, Some(start_opts)).await?;

        Ok(exec.id)
    }
}

// ---------------------------------------------------------------------------
// Helpers extracted from exec_interactive
// ---------------------------------------------------------------------------

/// Create an exec instance and start it in attached mode.
///
/// Returns `None` if the exec started in detached mode (shouldn't happen with
/// `detach: false`, but handles the enum variant gracefully).
async fn start_attached_exec(
    docker: &Docker,
    container_id: &str,
    create_opts: CreateExecOptions<&str>,
) -> Result<Option<(String, OutputStream, InputStream)>, CellaDockerError> {
    let exec = docker.create_exec(container_id, create_opts).await?;

    let start_opts = StartExecOptions {
        detach: false,
        ..Default::default()
    };

    let start_result = docker.start_exec(&exec.id, Some(start_opts)).await?;

    match start_result {
        StartExecResults::Attached { output, input } => Ok(Some((exec.id, output, input))),
        StartExecResults::Detached => Ok(None),
    }
}

/// Enable terminal raw mode and send an initial resize for TTY sessions.
///
/// Returns a [`RawModeGuard`] that restores the terminal on drop, or `None`
/// if TTY is disabled.
async fn enable_tty_raw_mode(
    docker: &Docker,
    exec_id: &str,
    tty: bool,
) -> Result<Option<RawModeGuard>, CellaDockerError> {
    if !tty {
        return Ok(None);
    }
    crossterm::terminal::enable_raw_mode()?;
    if let Ok((cols, rows)) = crossterm::terminal::size() {
        let _ = docker
            .resize_exec(
                exec_id,
                ResizeExecOptions {
                    width: cols,
                    height: rows,
                },
            )
            .await;
    }
    Ok(Some(RawModeGuard))
}

/// Spawn stdin-forwarding and output-forwarding tasks.
///
/// Returns `(stdin_handle, output_handle)`.
fn spawn_io_tasks(
    input: InputStream,
    mut output: OutputStream,
) -> (JoinHandle<()>, JoinHandle<()>) {
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

    let output_handle = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(chunk) = output.next().await {
            match chunk {
                Ok(
                    LogOutput::StdOut { message }
                    | LogOutput::StdErr { message }
                    | LogOutput::Console { message },
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

    (stdin_handle, output_handle)
}

/// Spawn a SIGWINCH handler that forwards terminal resize events to the exec
/// session.  Returns `None` if TTY is disabled.
#[cfg(unix)]
fn spawn_resize_handler(exec_id: &str, docker: &Docker, tty: bool) -> Option<JoinHandle<()>> {
    if !tty {
        return None;
    }
    let exec_id = exec_id.to_owned();
    let docker = docker.clone();
    Some(tokio::spawn(async move {
        let Ok(mut sig) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())
        else {
            return;
        };
        while sig.recv().await.is_some() {
            if let Ok((cols, rows)) = crossterm::terminal::size() {
                let _ = docker
                    .resize_exec(
                        &exec_id,
                        ResizeExecOptions {
                            width: cols,
                            height: rows,
                        },
                    )
                    .await;
            }
        }
    }))
}
