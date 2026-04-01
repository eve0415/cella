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

use self::run::{run_cli, run_cli_json, run_cli_owned};

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
        let output = run_cli_owned(&self.binary_path, &cli_args).await?;
        if output.exit_code != 0 {
            return Err(BackendError::Runtime(output.stderr.into()));
        }
        Ok(output.stdout.trim().to_string())
    }

    /// Start a stopped container.
    ///
    /// # Errors
    ///
    /// Returns an error if the CLI exits non-zero or cannot be spawned.
    pub async fn start(&self, id: &str) -> Result<(), BackendError> {
        let output = run_cli(&self.binary_path, &["start", id]).await?;
        if output.exit_code != 0 {
            return Err(BackendError::Runtime(output.stderr.into()));
        }
        Ok(())
    }

    /// Stop a running container.
    ///
    /// # Errors
    ///
    /// Returns an error if the CLI exits non-zero or cannot be spawned.
    pub async fn stop(&self, id: &str) -> Result<(), BackendError> {
        let output = run_cli(&self.binary_path, &["stop", id]).await?;
        if output.exit_code != 0 {
            return Err(BackendError::Runtime(output.stderr.into()));
        }
        Ok(())
    }

    /// Remove a container.
    ///
    /// # Errors
    ///
    /// Returns an error if the CLI exits non-zero or cannot be spawned.
    pub async fn rm(&self, id: &str) -> Result<(), BackendError> {
        let output = run_cli(&self.binary_path, &["rm", id]).await?;
        if output.exit_code != 0 {
            return Err(BackendError::Runtime(output.stderr.into()));
        }
        Ok(())
    }

    /// Inspect a container and return its full metadata.
    ///
    /// # Errors
    ///
    /// Returns an error if the container does not exist, the CLI exits
    /// non-zero, or the JSON output cannot be parsed.
    pub async fn inspect(&self, id: &str) -> Result<types::ContainerInspect, BackendError> {
        run_cli_json(&self.binary_path, &["inspect", id, "--format", "json"]).await
    }

    /// List containers, optionally filtering by a label.
    ///
    /// # Errors
    ///
    /// Returns an error if the CLI exits non-zero or JSON parsing fails.
    pub async fn list(
        &self,
        label_filter: Option<&str>,
    ) -> Result<Vec<types::ContainerListEntry>, BackendError> {
        let mut args = vec!["ls", "--format", "json", "--all"];
        if let Some(label) = label_filter {
            args.push("--filter");
            args.push(label);
        }
        run_cli_json(&self.binary_path, &args).await
    }

    /// Fetch the last `tail` lines of container logs.
    ///
    /// # Errors
    ///
    /// Returns an error if the CLI cannot be spawned.
    pub async fn logs(&self, id: &str, tail: u32) -> Result<String, BackendError> {
        let tail_str = tail.to_string();
        let output = run_cli(&self.binary_path, &["logs", id, "--tail", &tail_str]).await?;
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
        let output = run_cli(&self.binary_path, &["image", "pull", image]).await?;
        if output.exit_code != 0 {
            return Err(BackendError::Runtime(output.stderr.into()));
        }
        Ok(())
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
        let mut cli_args = vec![
            "build".to_string(),
            "-f".to_string(),
            dockerfile.to_string(),
            "-t".to_string(),
            tag.to_string(),
        ];
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

    /// Inspect an image and return raw JSON output.
    ///
    /// # Errors
    ///
    /// Returns `BackendError::ImageNotFound` if the image does not exist.
    pub async fn image_inspect(&self, image: &str) -> Result<String, BackendError> {
        let output = run_cli(
            &self.binary_path,
            &["image", "inspect", image, "--format", "json"],
        )
        .await?;
        if output.exit_code != 0 {
            return Err(BackendError::ImageNotFound {
                image: image.to_string(),
            });
        }
        Ok(output.stdout)
    }

    /// Create a named volume.
    ///
    /// # Errors
    ///
    /// Returns an error if the CLI exits non-zero or cannot be spawned.
    pub async fn volume_create(&self, name: &str) -> Result<(), BackendError> {
        let output = run_cli(&self.binary_path, &["volume", "create", name]).await?;
        if output.exit_code != 0 {
            return Err(BackendError::Runtime(output.stderr.into()));
        }
        Ok(())
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
                let path = dir.join(name);
                let content = format!("#!/bin/sh\n{body}\n");
                // Only write if missing or content changed; avoids ETXTBSY
                // when another thread is executing the same file.
                let needs_write = std::fs::read_to_string(&path)
                    .map_or(true, |existing| existing != content);
                if needs_write {
                    std::fs::write(&path, &content).unwrap();
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(
                        &path,
                        std::fs::Permissions::from_mode(0o755),
                    );
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
                    r#"echo '{"status":{"state":"running"},"configuration":{"id":"x","name":"n"}}'"#,
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

    // -- list -----------------------------------------------------------------

    #[tokio::test]
    async fn list_parses_valid_json_array() {
        let cli = cli_from(&mock_scripts().list_json);
        let result = cli.list(None).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn list_with_label_filter() {
        let cli = cli_from(&mock_scripts().empty_list);
        let result = cli.list(Some("dev.cella.tool=cella")).await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_empty());
    }

    #[tokio::test]
    async fn list_error_on_invalid_json() {
        let cli = echo_cli();
        let result = cli.list(None).await;
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

    // -- volume_create --------------------------------------------------------

    #[tokio::test]
    async fn volume_create_succeeds_with_echo() {
        let cli = echo_cli();
        let result = cli.volume_create("my-volume").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn volume_create_error_on_nonzero_exit() {
        let cli = cli_from(&mock_scripts().fail);
        let result = cli.volume_create("my-volume").await;
        assert!(result.is_err());
    }

    // -- nonexistent binary ---------------------------------------------------

    #[tokio::test]
    async fn create_error_on_missing_binary() {
        let cli = ContainerCli::new(PathBuf::from("/nonexistent/binary"), "v0".to_string());
        let result = cli.create(&[]).await;
        assert!(result.is_err());
    }
}
