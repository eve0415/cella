//! SDK for driving the Apple `container` CLI binary.
//!
//! [`ContainerCli`] wraps the binary and provides typed async methods
//! for each CLI subcommand. All operations shell out to the binary and
//! parse its stdout/stderr.

pub mod run;
pub mod types;

use std::path::{Path, PathBuf};

use cella_backend::BackendError;
use tracing::debug;

use self::run::{run_cli, run_cli_checked, run_cli_checked_owned, run_cli_json, run_cli_owned};

/// Handle to a discovered Apple Container CLI binary.
pub struct ContainerCli {
    binary_path: PathBuf,
    version: String,
}

impl ContainerCli {
    /// Create a new handle from a discovered binary path and version string.
    pub const fn new(binary_path: PathBuf, version: String) -> Self {
        Self {
            binary_path,
            version,
        }
    }

    /// Path to the `container` binary.
    pub fn binary_path(&self) -> &Path {
        &self.binary_path
    }

    /// Discovered version string.
    pub fn version(&self) -> &str {
        &self.version
    }

    // -- Container lifecycle operations --

    /// Create a container (without starting it).
    ///
    /// `args` should contain all flags and the image name as the final element.
    /// Returns the container ID (plain text from stdout).
    ///
    /// # Errors
    ///
    /// Returns an error if the CLI exits non-zero or cannot be spawned.
    pub async fn create(&self, args: &[String]) -> Result<String, BackendError> {
        let mut cli_args = vec!["create".to_string()];
        cli_args.extend_from_slice(args);
        let output = run_cli_checked_owned(&self.binary_path, &cli_args).await?;
        Ok(output.stdout.trim().to_string())
    }

    /// Start a stopped container.
    ///
    /// # Errors
    ///
    /// Returns an error if the CLI exits non-zero or cannot be spawned.
    pub async fn start(&self, id: &str) -> Result<(), BackendError> {
        run_cli_checked(&self.binary_path, &["start", id])
            .await
            .map(drop)
    }

    /// Stop a running container.
    ///
    /// # Errors
    ///
    /// Returns an error if the CLI exits non-zero or cannot be spawned.
    pub async fn stop(&self, id: &str) -> Result<(), BackendError> {
        run_cli_checked(&self.binary_path, &["stop", id])
            .await
            .map(drop)
    }

    /// Remove a container.
    ///
    /// # Errors
    ///
    /// Returns an error if the CLI exits non-zero or cannot be spawned.
    pub async fn rm(&self, id: &str) -> Result<(), BackendError> {
        run_cli_checked(&self.binary_path, &["rm", id])
            .await
            .map(drop)
    }

    /// Inspect a container and return its full metadata.
    ///
    /// `container inspect` always emits a JSON array; the first entry is
    /// returned.
    ///
    /// # Errors
    ///
    /// Returns an error if the container does not exist, the CLI exits
    /// non-zero, or the JSON output cannot be parsed.
    pub async fn inspect(&self, id: &str) -> Result<types::ContainerInspect, BackendError> {
        let entries: Vec<types::ContainerInspect> =
            run_cli_json(&self.binary_path, &["inspect", id]).await?;
        entries
            .into_iter()
            .next()
            .ok_or_else(|| BackendError::ContainerNotFound {
                identifier: id.to_string(),
            })
    }

    /// List all containers (running and stopped).
    ///
    /// The CLI has no server-side filtering; callers filter on labels from
    /// the returned entries.
    ///
    /// # Errors
    ///
    /// Returns an error if the CLI exits non-zero or JSON parsing fails.
    pub async fn list(&self) -> Result<Vec<types::ContainerListEntry>, BackendError> {
        run_cli_json(&self.binary_path, &["ls", "--format", "json", "--all"]).await
    }

    /// Fetch the last `tail` lines of container logs.
    ///
    /// # Errors
    ///
    /// Returns an error if the CLI cannot be spawned.
    pub async fn logs(&self, id: &str, tail: u32) -> Result<String, BackendError> {
        let tail_str = tail.to_string();
        let output = run_cli(&self.binary_path, &["logs", id, "-n", &tail_str]).await?;
        // Logs may come on stderr for some runtimes; combine both.
        let mut combined = output.stdout;
        if !output.stderr.is_empty() {
            if !combined.is_empty() {
                combined.push('\n');
            }
            combined.push_str(&output.stderr);
        }
        Ok(combined)
    }

    // -- Exec operations --

    /// Execute a command inside a container and capture its output.
    ///
    /// Returns `(exit_code, stdout, stderr)`.
    ///
    /// # Errors
    ///
    /// Returns an error if the CLI cannot be spawned.
    pub async fn exec_capture(
        &self,
        id: &str,
        cmd: &[String],
        user: Option<&str>,
        env: Option<&[String]>,
        workdir: Option<&str>,
    ) -> Result<(i64, String, String), BackendError> {
        let mut args = vec!["exec".to_string()];

        if let Some(u) = user {
            args.push("--user".to_string());
            args.push(u.to_string());
        }
        if let Some(vars) = env {
            for var in vars {
                args.push("-e".to_string());
                args.push(var.clone());
            }
        }
        if let Some(wd) = workdir {
            args.push("-w".to_string());
            args.push(wd.to_string());
        }

        args.push(id.to_string());
        for c in cmd {
            args.push(c.clone());
        }

        let output = run_cli_owned(&self.binary_path, &args).await?;
        let exit_code = i64::from(output.exit_code);
        Ok((exit_code, output.stdout, output.stderr))
    }

    // -- Image operations --

    /// Pull an image from a registry.
    ///
    /// # Errors
    ///
    /// Returns an error if the CLI exits non-zero or cannot be spawned.
    pub async fn pull(&self, image: &str) -> Result<(), BackendError> {
        debug!(image, "pulling image");
        run_cli_checked(&self.binary_path, &["image", "pull", image])
            .await
            .map(drop)
    }

    /// Build an image from a Dockerfile.
    ///
    /// Returns the image tag.
    ///
    /// # Errors
    ///
    /// Returns `BackendError::ImageBuildFailed` if the build exits non-zero.
    pub async fn build(
        &self,
        context: &Path,
        dockerfile: &str,
        tag: &str,
        args: &[(String, String)],
    ) -> Result<String, BackendError> {
        self.build_with_extra_args(context, dockerfile, tag, args, &[])
            .await
    }

    /// Build an image with additional CLI flags (target, cache, options).
    ///
    /// # Errors
    ///
    /// Returns `BackendError::ImageBuildFailed` if the build exits non-zero.
    pub async fn build_with_extra_args(
        &self,
        context: &Path,
        dockerfile: &str,
        tag: &str,
        args: &[(String, String)],
        extra_args: &[String],
    ) -> Result<String, BackendError> {
        let mut cli_args = vec![
            "build".to_string(),
            "-f".to_string(),
            dockerfile.to_string(),
            "-t".to_string(),
            tag.to_string(),
        ];
        cli_args.extend_from_slice(extra_args);
        for (key, value) in args {
            cli_args.push("--build-arg".to_string());
            cli_args.push(format!("{key}={value}"));
        }
        cli_args.push(context.to_string_lossy().into_owned());

        let output = run_cli_owned(&self.binary_path, &cli_args).await?;
        if output.exit_code != 0 {
            return Err(BackendError::ImageBuildFailed {
                message: output.stderr,
            });
        }
        Ok(tag.to_string())
    }

    /// Check whether an image exists locally.
    ///
    /// # Errors
    ///
    /// Returns an error if the CLI cannot be spawned.
    pub async fn image_exists(&self, image: &str) -> Result<bool, BackendError> {
        let output = run_cli(&self.binary_path, &["image", "inspect", image]).await?;
        Ok(output.exit_code == 0)
    }

    /// Inspect an image and return raw JSON output (an array of image
    /// resources).
    ///
    /// # Errors
    ///
    /// Returns `BackendError::ImageNotFound` if the image does not exist.
    pub async fn image_inspect(&self, image: &str) -> Result<String, BackendError> {
        let output = run_cli(&self.binary_path, &["image", "inspect", image]).await?;
        if output.exit_code != 0 {
            return Err(BackendError::ImageNotFound {
                image: image.to_string(),
            });
        }
        Ok(output.stdout)
    }

    /// Copy a file from the host into a running container.
    ///
    /// Wraps `container cp <host_path> <id>:<container_path>`.
    ///
    /// # Errors
    ///
    /// Returns an error if the CLI exits non-zero or cannot be spawned.
    pub async fn cp_into(
        &self,
        host_path: &Path,
        id: &str,
        container_path: &str,
    ) -> Result<(), BackendError> {
        let args = vec![
            "cp".to_string(),
            host_path.to_string_lossy().into_owned(),
            format!("{id}:{container_path}"),
        ];
        run_cli_checked_owned(&self.binary_path, &args)
            .await
            .map(drop)
            .map_err(|e| {
                BackendError::Runtime(
                    format!("container cp to {container_path} failed: {e}").into(),
                )
            })
    }

    // -- Network operations (macOS 26+) --

    /// Create a network with the given labels.
    ///
    /// # Errors
    ///
    /// Returns an error if the CLI exits non-zero (including on macOS 15,
    /// where the network commands do not exist) or cannot be spawned.
    pub async fn network_create(
        &self,
        name: &str,
        labels: &[(&str, &str)],
    ) -> Result<(), BackendError> {
        let mut args = vec!["network".to_string(), "create".to_string()];
        for (key, value) in labels {
            args.push("--label".to_string());
            args.push(format!("{key}={value}"));
        }
        args.push(name.to_string());

        run_cli_checked_owned(&self.binary_path, &args)
            .await
            .map(drop)
            .map_err(|e| BackendError::Runtime(format!("network create {name} failed: {e}").into()))
    }

    /// List networks.
    ///
    /// # Errors
    ///
    /// Returns an error if the CLI exits non-zero (including on macOS 15,
    /// where the network commands do not exist) or JSON parsing fails.
    pub async fn network_list(&self) -> Result<Vec<types::NetworkListEntry>, BackendError> {
        run_cli_json(&self.binary_path, &["network", "ls", "--format", "json"]).await
    }

    /// Delete a network by name.
    ///
    /// # Errors
    ///
    /// Returns an error if the CLI exits non-zero or cannot be spawned.
    pub async fn network_delete(&self, name: &str) -> Result<(), BackendError> {
        run_cli_checked(&self.binary_path, &["network", "delete", name])
            .await
            .map(drop)
            .map_err(|e| BackendError::Runtime(format!("network delete {name} failed: {e}").into()))
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    /// Helper: build a `ContainerCli` backed by `/bin/echo` so every
    /// subcommand succeeds (exit 0) and prints its arguments to stdout.
    fn echo_cli() -> ContainerCli {
        ContainerCli::new(PathBuf::from("/bin/echo"), "test-1.0.0".to_string())
    }

    /// Pre-built mock scripts. Created once (lazily) before any test uses
    /// them, so we never write a script file while another thread is
    /// executing one -- which avoids `ETXTBSY` on overlayfs.
    struct MockScripts {
        /// `exit 1` (simulates CLI failure).
        fail: PathBuf,
        /// Prints `"something went wrong"` to stderr, exits 1.
        fail_stderr: PathBuf,
        /// Prints `"build failed"` to stderr, exits 1.
        build_fail: PathBuf,
        /// Outputs valid JSON for a single container inspect entry.
        inspect_json: PathBuf,
        /// Outputs valid JSON for a container list (one entry).
        list_json: PathBuf,
        /// Outputs `[]` (empty JSON array).
        empty_list: PathBuf,
        /// Outputs `{"Config":{}}`.
        image_json: PathBuf,
        /// Prints one line to stdout and one to stderr.
        both_streams: PathBuf,
    }

    fn mock_scripts() -> &'static MockScripts {
        use std::sync::OnceLock;

        static SCRIPTS: OnceLock<MockScripts> = OnceLock::new();
        SCRIPTS.get_or_init(|| {
            let dir = PathBuf::from("/tmp/cella_sdk_mock_scripts");
            std::fs::create_dir_all(&dir).unwrap();

            let write_script = |name: &str, body: &str| -> PathBuf {
                use std::io::Write;
                let path = dir.join(name);
                let content = format!("#!/bin/sh\n{body}\n");
                // Only write if missing or content changed; avoids ETXTBSY
                // when another thread is executing the same file.
                let needs_write =
                    std::fs::read_to_string(&path).map_or(true, |existing| existing != content);
                if needs_write {
                    let mut file = std::fs::File::create(&path).unwrap();
                    file.write_all(content.as_bytes()).unwrap();
                    file.sync_all().unwrap();
                    drop(file);
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755));
                }
                // ETXTBSY guard: the kernel may not have released the inode
                // write reference yet (deferred __fput). Spin until exec works.
                if needs_write {
                    for _ in 0..50 {
                        match std::process::Command::new(&path)
                            .arg("--etxtbsy-probe")
                            .stdout(std::process::Stdio::null())
                            .stderr(std::process::Stdio::null())
                            .output()
                        {
                            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                                std::thread::sleep(std::time::Duration::from_millis(1));
                            }
                            _ => break,
                        }
                    }
                }
                path
            };

            MockScripts {
                fail: write_script("fail.sh", "exit 1"),
                fail_stderr: write_script(
                    "fail_stderr.sh",
                    "echo 'something went wrong' >&2; exit 1",
                ),
                build_fail: write_script("build_fail.sh", "echo 'build failed' >&2; exit 1"),
                inspect_json: write_script(
                    "inspect_json.sh",
                    r#"echo '[{"status":{"state":"running"},"configuration":{"id":"x"}}]'"#,
                ),
                list_json: write_script(
                    "list_json.sh",
                    r#"echo '[{"status":{"state":"running"},"configuration":{"id":"a"}}]'"#,
                ),
                empty_list: write_script("empty_list.sh", r"echo '[]'"),
                image_json: write_script("image_json.sh", r#"echo '{"Config":{}}'"#),
                both_streams: write_script(
                    "both_streams.sh",
                    "echo 'stdout-line'; echo 'stderr-line' >&2",
                ),
            }
        })
    }

    /// Helper: build a `ContainerCli` backed by a pre-built mock script.
    fn cli_from(script: &Path) -> ContainerCli {
        ContainerCli::new(script.to_path_buf(), "mock-1.0.0".to_string())
    }

    // -- Accessor tests -------------------------------------------------------

    #[test]
    fn binary_path_returns_correct_path() {
        let cli = echo_cli();
        assert_eq!(cli.binary_path(), Path::new("/bin/echo"));
    }

    #[test]
    fn version_returns_correct_string() {
        let cli = echo_cli();
        assert_eq!(cli.version(), "test-1.0.0");
    }

    // -- create ---------------------------------------------------------------

    #[tokio::test]
    async fn create_returns_stdout_trimmed() {
        let cli = echo_cli();
        let args = vec![
            "--name".to_string(),
            "mycontainer".to_string(),
            "ubuntu:latest".to_string(),
        ];
        let result = cli.create(&args).await.unwrap();
        // /bin/echo will print: create --name mycontainer ubuntu:latest
        assert!(result.contains("create"));
        assert!(result.contains("mycontainer"));
    }

    #[tokio::test]
    async fn create_error_on_nonzero_exit() {
        let cli = cli_from(&mock_scripts().fail_stderr);
        let result = cli.create(&[]).await;
        assert!(result.is_err());
    }

    // -- start ----------------------------------------------------------------

    #[tokio::test]
    async fn start_succeeds_with_echo() {
        let cli = echo_cli();
        let result = cli.start("container-id-123").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn start_error_on_nonzero_exit() {
        let cli = cli_from(&mock_scripts().fail);
        let result = cli.start("abc").await;
        assert!(result.is_err());
    }

    // -- stop -----------------------------------------------------------------

    #[tokio::test]
    async fn stop_succeeds_with_echo() {
        let cli = echo_cli();
        let result = cli.stop("container-id-123").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn stop_error_on_nonzero_exit() {
        let cli = cli_from(&mock_scripts().fail);
        let result = cli.stop("abc").await;
        assert!(result.is_err());
    }

    // -- rm -------------------------------------------------------------------

    #[tokio::test]
    async fn rm_succeeds_with_echo() {
        let cli = echo_cli();
        let result = cli.rm("container-id-123").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn rm_error_on_nonzero_exit() {
        let cli = cli_from(&mock_scripts().fail);
        let result = cli.rm("abc").await;
        assert!(result.is_err());
    }

    // -- inspect --------------------------------------------------------------

    #[tokio::test]
    async fn inspect_parses_valid_json() {
        let cli = cli_from(&mock_scripts().inspect_json);
        let result = cli.inspect("x").await;
        assert!(result.is_ok());
        let entry = result.unwrap();
        assert_eq!(
            entry.configuration.as_ref().unwrap().id.as_deref(),
            Some("x")
        );
    }

    #[tokio::test]
    async fn inspect_error_on_invalid_json() {
        // /bin/echo outputs its args as plain text, not JSON
        let cli = echo_cli();
        let result = cli.inspect("some-id").await;
        assert!(result.is_err(), "expected JSON parse error");
    }

    #[tokio::test]
    async fn inspect_empty_array_is_not_found() {
        let cli = cli_from(&mock_scripts().empty_list);
        let result = cli.inspect("ghost").await;
        assert!(
            matches!(result, Err(BackendError::ContainerNotFound { .. })),
            "expected ContainerNotFound for empty inspect output"
        );
    }

    // -- list -----------------------------------------------------------------

    #[tokio::test]
    async fn list_parses_valid_json_array() {
        let cli = cli_from(&mock_scripts().list_json);
        let result = cli.list().await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn list_empty_array() {
        let cli = cli_from(&mock_scripts().empty_list);
        let result = cli.list().await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_empty());
    }

    #[tokio::test]
    async fn list_error_on_invalid_json() {
        let cli = echo_cli();
        let result = cli.list().await;
        assert!(result.is_err(), "expected JSON parse error");
    }

    // -- logs -----------------------------------------------------------------

    #[tokio::test]
    async fn logs_returns_stdout() {
        let cli = echo_cli();
        let result = cli.logs("cid", 100).await.unwrap();
        assert!(result.contains("logs"));
        assert!(result.contains("cid"));
    }

    #[tokio::test]
    async fn logs_combines_stdout_and_stderr() {
        let cli = cli_from(&mock_scripts().both_streams);
        let result = cli.logs("cid", 50).await.unwrap();
        assert!(result.contains("stdout-line"));
        assert!(result.contains("stderr-line"));
    }

    // -- exec_capture ---------------------------------------------------------

    #[tokio::test]
    async fn exec_capture_basic() {
        let cli = echo_cli();
        let cmd = vec!["ls".to_string(), "-la".to_string()];
        let (exit_code, stdout, _stderr) = cli
            .exec_capture("cid", &cmd, None, None, None)
            .await
            .unwrap();
        assert_eq!(exit_code, 0);
        assert!(stdout.contains("exec"));
        assert!(stdout.contains("ls"));
    }

    #[tokio::test]
    async fn exec_capture_with_user_env_workdir() {
        let cli = echo_cli();
        let cmd = vec!["whoami".to_string()];
        let env = vec!["FOO=bar".to_string()];
        let (exit_code, stdout, _) = cli
            .exec_capture("cid", &cmd, Some("root"), Some(&env), Some("/tmp"))
            .await
            .unwrap();
        assert_eq!(exit_code, 0);
        assert!(stdout.contains("--user"));
        assert!(stdout.contains("root"));
        assert!(stdout.contains("-e"));
        assert!(stdout.contains("FOO=bar"));
        assert!(stdout.contains("-w"));
        assert!(stdout.contains("/tmp"));
    }

    // -- pull -----------------------------------------------------------------

    #[tokio::test]
    async fn pull_succeeds_with_echo() {
        let cli = echo_cli();
        let result = cli.pull("ubuntu:latest").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn pull_error_on_nonzero_exit() {
        let cli = cli_from(&mock_scripts().fail);
        let result = cli.pull("ubuntu:latest").await;
        assert!(result.is_err());
    }

    // -- build ----------------------------------------------------------------

    #[tokio::test]
    async fn build_returns_tag_on_success() {
        let cli = echo_cli();
        let result = cli
            .build(
                Path::new("/tmp/ctx"),
                "Dockerfile",
                "myimage:latest",
                &[("ARG1".to_string(), "val1".to_string())],
            )
            .await
            .unwrap();
        assert_eq!(result, "myimage:latest");
    }

    #[tokio::test]
    async fn build_error_on_nonzero_exit() {
        let cli = cli_from(&mock_scripts().build_fail);
        let result = cli
            .build(Path::new("/tmp"), "Dockerfile", "img:v1", &[])
            .await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, BackendError::ImageBuildFailed { .. }),
            "expected ImageBuildFailed, got: {err:?}"
        );
    }

    // -- image_exists ---------------------------------------------------------

    #[tokio::test]
    async fn image_exists_returns_true_on_zero_exit() {
        let cli = echo_cli();
        let result = cli.image_exists("ubuntu:latest").await.unwrap();
        assert!(result);
    }

    #[tokio::test]
    async fn image_exists_returns_false_on_nonzero_exit() {
        let cli = cli_from(&mock_scripts().fail);
        let result = cli.image_exists("nonexistent:latest").await.unwrap();
        assert!(!result);
    }

    // -- image_inspect --------------------------------------------------------

    #[tokio::test]
    async fn image_inspect_returns_stdout() {
        let cli = cli_from(&mock_scripts().image_json);
        let result = cli.image_inspect("ubuntu:latest").await.unwrap();
        assert!(result.contains("Config"));
    }

    #[tokio::test]
    async fn image_inspect_error_on_nonzero_exit() {
        let cli = cli_from(&mock_scripts().fail);
        let result = cli.image_inspect("nonexistent").await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, BackendError::ImageNotFound { .. }),
            "expected ImageNotFound, got: {err:?}"
        );
    }

    // -- nonexistent binary ---------------------------------------------------

    #[tokio::test]
    async fn create_error_on_missing_binary() {
        let cli = ContainerCli::new(PathBuf::from("/nonexistent/binary"), "v0".to_string());
        let result = cli.create(&[]).await;
        assert!(result.is_err());
    }
}
