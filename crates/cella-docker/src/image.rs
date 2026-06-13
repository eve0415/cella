//! Image pull and Dockerfile build operations.

use std::fmt::Write as _;
use std::process::Stdio;

use bollard::query_parameters::CreateImageOptions;
use futures_util::StreamExt;
use tracing::{debug, info};

use cella_backend::{BuildOptions, ImageDetails};

use crate::CellaDockerError;
use crate::client::DockerClient;

/// Extract the user/uid portion from a Docker USER value.
///
/// Docker USER can be `"user"`, `"user:group"`, `"uid"`, or `"uid:gid"`.
/// Returns just the user/uid part, or `"root"` if empty.
pub(crate) fn normalize_user(raw: &str) -> String {
    let user = raw.split(':').next().unwrap_or("");
    if user.is_empty() {
        "root".to_string()
    } else {
        user.to_string()
    }
}

/// Resolve the docker binary path (defaults to `docker` on `PATH`).
///
/// Unlike the previous process-global cache, this re-probes per call so a
/// per-invocation `--docker-path` override is honored. Builds are infrequent,
/// so the extra `--version` probe is negligible.
fn docker_binary(docker_path: Option<&str>) -> Result<String, CellaDockerError> {
    let bin = docker_path.unwrap_or("docker");
    let output = std::process::Command::new(bin).arg("--version").output();
    match output {
        Ok(o) if o.status.success() => Ok(bin.to_string()),
        Ok(o) => Err(CellaDockerError::DockerCliNotFound {
            message: format!(
                "{bin} --version failed: {}",
                String::from_utf8_lossy(&o.stderr)
            ),
        }),
        Err(e) => Err(CellaDockerError::DockerCliNotFound {
            message: format!("{bin} not found: {e}"),
        }),
    }
}

/// Check whether `<docker> buildx` is available.
///
/// Re-probes per call (no global cache) so the result tracks `docker_path`.
fn has_buildx(docker_path: Option<&str>) -> bool {
    let bin = docker_path.unwrap_or("docker");
    std::process::Command::new(bin)
        .args(["buildx", "version"])
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Whether a `--cache-to` value already exports an inline cache.
///
/// Matches the official `isBuildxCacheToInline` regex `/type\s*=\s*inline/i`
/// (case-insensitive, whitespace-tolerant) with a hand-rolled scan to avoid a
/// new dependency. When true, the separate `BUILDKIT_INLINE_CACHE=1` build-arg
/// is skipped (the cache-to export already inlines).
fn is_cache_to_inline(cache_to: &str) -> bool {
    let lower = cache_to.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut i = 0;
    while let Some(pos) = lower[i..].find("type") {
        let mut j = i + pos + "type".len();
        // Skip whitespace, then require '='.
        while bytes.get(j).is_some_and(u8::is_ascii_whitespace) {
            j += 1;
        }
        if bytes.get(j) == Some(&b'=') {
            j += 1;
            while bytes.get(j).is_some_and(u8::is_ascii_whitespace) {
                j += 1;
            }
            if lower[j..].starts_with("inline") {
                return true;
            }
        }
        i += pos + "type".len();
    }
    false
}

/// Build the argument list for a `docker [buildx] build` invocation.
fn build_command_args(opts: &BuildOptions, use_buildx: bool) -> Vec<String> {
    let mut args = Vec::new();

    if use_buildx {
        args.push("buildx".to_string());
    }
    args.push("build".to_string());

    // `--progress=plain` and `--load` are buildx-only. The classic builder
    // rejects `--progress`, and official emits neither on the legacy path.
    if use_buildx {
        args.push("--progress=plain".to_string());
        args.push("--load".to_string());
    }

    if let Some(platform) = &opts.platform {
        args.extend(["--platform".to_string(), platform.clone()]);
    }

    args.extend(["-t".to_string(), opts.image_name.clone()]);

    args.extend([
        "-f".to_string(),
        opts.context_path
            .join(&opts.dockerfile)
            .to_string_lossy()
            .to_string(),
    ]);

    for (k, v) in &opts.args {
        args.extend(["--build-arg".to_string(), format!("{k}={v}")]);
    }

    if let Some(target) = &opts.target {
        args.extend(["--target".to_string(), target.clone()]);
    }

    for cf in &opts.cache_from {
        args.extend(["--cache-from".to_string(), cf.clone()]);
    }

    // `--cache-to` is buildx-only; silently dropped on the legacy path
    // (matching the official CLI). When set and not already an inline export,
    // also request the inline-cache build-arg so the resulting image carries
    // its layer cache metadata.
    if use_buildx && let Some(cache_to) = &opts.cache_to {
        args.extend(["--cache-to".to_string(), cache_to.clone()]);
        if !is_cache_to_inline(cache_to) {
            args.extend([
                "--build-arg".to_string(),
                "BUILDKIT_INLINE_CACHE=1".to_string(),
            ]);
        }
    }

    for opt in &opts.options {
        args.push(opt.clone());
    }

    for secret in &opts.secrets {
        let mut spec = format!("id={}", secret.id);
        if let Some(ref src) = secret.src {
            let _ = write!(spec, ",src={}", src.display());
        }
        if let Some(ref env) = secret.env {
            let _ = write!(spec, ",env={env}");
        }
        args.extend(["--secret".to_string(), spec]);
    }

    args.push(opts.context_path.to_string_lossy().to_string());
    args
}

/// Spawn a task that reads lines from an async stream and forwards them to a channel.
fn spawn_line_reader(
    stream: impl tokio::io::AsyncRead + Unpin + Send + 'static,
    tx: tokio::sync::mpsc::UnboundedSender<String>,
) {
    tokio::spawn(async move {
        use tokio::io::{AsyncBufReadExt, BufReader};
        let mut lines = BufReader::new(stream).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            // Split on \r to handle carriage-return progress updates
            // (BuildKit, apt-get, wget). Each segment becomes its own line.
            for segment in line.split('\r') {
                if !segment.is_empty() {
                    let _ = tx.send(segment.to_string());
                }
            }
        }
    });
}

impl DockerClient {
    /// Pull an image by reference (e.g., `mcr.microsoft.com/devcontainers/rust:1`).
    ///
    /// # Errors
    ///
    /// Returns `CellaDockerError::DockerApi` if the pull fails.
    pub async fn pull_image(&self, image: &str) -> Result<(), CellaDockerError> {
        info!("Pulling image: {image}");

        let options = CreateImageOptions {
            from_image: Some(image.to_string()),
            ..Default::default()
        };

        let mut stream = self.inner().create_image(Some(options), None, None);
        while let Some(result) = stream.next().await {
            match result {
                Ok(info) => {
                    if let Some(status) = &info.status {
                        debug!("{status}");
                    }
                }
                Err(e) => return Err(CellaDockerError::DockerApi(e)),
            }
        }

        info!("Image pulled: {image}");
        Ok(())
    }

    /// Build an image from a Dockerfile using the docker CLI.
    ///
    /// Uses `<docker> buildx build` when `BuildKit` is enabled (`use_buildkit`)
    /// AND buildx is present; otherwise runs the classic `<docker> build`. The
    /// `docker` binary is taken from `opts.docker_path` when set. No
    /// `DOCKER_BUILDKIT`/`BUILDKIT_PROGRESS` env is set — the build subcommand
    /// (`buildx build` vs `build`) is the sole `BuildKit` selector, matching
    /// the official CLI.
    ///
    /// Build output (stdout/stderr) is captured and forwarded to the
    /// provided callback line by line. Pass `|_| {}` to discard output.
    ///
    /// # Errors
    ///
    /// Returns error if the docker CLI is not found or the build fails.
    pub async fn build_image(
        &self,
        opts: &BuildOptions,
        mut on_output: impl FnMut(&str),
    ) -> Result<String, CellaDockerError> {
        info!("Building image: {}", opts.image_name);

        let docker_path = opts.docker_path.as_deref();
        let bin = docker_binary(docker_path)?;
        // `auto` probes for buildx; `never` (use_buildkit == false) forces the
        // classic builder without probing.
        let use_buildx = opts.use_buildkit && has_buildx(docker_path);

        // `--cache-to` is buildx-only. On the classic builder (BuildKit never,
        // or auto with no buildx) it is silently dropped from the command, so
        // warn the user once that it had no effect. cache_to is only ever set
        // on the base build's BuildOptions, so this fires at most once per up.
        if !use_buildx && opts.cache_to.is_some() {
            tracing::warn!("--cache-to is ignored without BuildKit/buildx (classic docker build)");
        }

        let args = build_command_args(opts, use_buildx);

        debug!("{bin} {}", args.join(" "));

        let mut cmd = tokio::process::Command::new(&bin);
        cmd.args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd
            .spawn()
            .map_err(|source| CellaDockerError::HostCommandFailed {
                command: format!("{bin} {}", args.join(" ")),
                source,
            })?;

        // Stream stdout and stderr lines in real-time via a channel.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();

        if let Some(stdout) = child.stdout.take() {
            spawn_line_reader(stdout, tx.clone());
        }

        if let Some(stderr) = child.stderr.take() {
            spawn_line_reader(stderr, tx.clone());
        }

        // Drop our sender so rx closes when spawned tasks finish.
        drop(tx);

        // Forward lines to the callback as they arrive.
        while let Some(line) = rx.recv().await {
            on_output(&line);
        }

        let status = child
            .wait()
            .await
            .map_err(|source| CellaDockerError::HostCommandFailed {
                command: format!("{bin} {}", args.join(" ")),
                source,
            })?;

        if !status.success() {
            return Err(CellaDockerError::BuildFailed {
                message: format!(
                    "docker build exited with code {}",
                    status.code().unwrap_or(-1)
                ),
            });
        }

        info!("Image built: {}", opts.image_name);
        Ok(opts.image_name.clone())
    }

    /// Check if an image exists locally.
    ///
    /// # Errors
    ///
    /// Returns `CellaDockerError::DockerApi` on API errors (not for 404).
    pub async fn image_exists(&self, image: &str) -> Result<bool, CellaDockerError> {
        match self.inner().inspect_image(image).await {
            Ok(_) => Ok(true),
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => Ok(false),
            Err(e) => Err(CellaDockerError::DockerApi(e)),
        }
    }

    /// Inspect an image and return its user, env, and metadata in one API call.
    ///
    /// # Errors
    ///
    /// Returns `CellaDockerError::DockerApi` on API errors,
    /// `CellaDockerError::ImageNotFound` if the image does not exist.
    pub async fn inspect_image_details(
        &self,
        image: &str,
    ) -> Result<ImageDetails, CellaDockerError> {
        match self.inner().inspect_image(image).await {
            Ok(details) => {
                let config = details.config.as_ref();
                let user = normalize_user(config.and_then(|c| c.user.as_deref()).unwrap_or(""));
                let env = details
                    .config
                    .as_ref()
                    .and_then(|c| c.env.clone())
                    .unwrap_or_default();
                let metadata = config
                    .and_then(|c| c.labels.as_ref())
                    .and_then(|labels| labels.get("devcontainer.metadata").cloned());
                Ok(ImageDetails {
                    user,
                    env,
                    metadata,
                    os: details.os.clone(),
                    architecture: details.architecture.clone(),
                    variant: details.variant.clone(),
                })
            }
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => Err(CellaDockerError::ImageNotFound {
                image: image.to_string(),
            }),
            Err(e) => Err(CellaDockerError::DockerApi(e)),
        }
    }

    /// Convenience: inspect an image and return only its environment variables.
    ///
    /// # Errors
    ///
    /// Returns `CellaDockerError::DockerApi` on API errors,
    /// `CellaDockerError::ImageNotFound` if the image does not exist.
    pub async fn inspect_image_env(&self, image: &str) -> Result<Vec<String>, CellaDockerError> {
        self.inspect_image_details(image).await.map(|d| d.env)
    }

    /// Convenience: inspect an image and return only its USER (defaulting to `"root"`).
    ///
    /// # Errors
    ///
    /// Returns `CellaDockerError::DockerApi` on API errors,
    /// `CellaDockerError::ImageNotFound` if the image does not exist.
    pub async fn inspect_image_user(&self, image: &str) -> Result<String, CellaDockerError> {
        self.inspect_image_details(image).await.map(|d| d.user)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;

    use super::*;

    fn basic_opts() -> BuildOptions {
        BuildOptions {
            image_name: "myimage:latest".to_string(),
            context_path: PathBuf::from("/src/project"),
            dockerfile: "Dockerfile".to_string(),
            args: HashMap::new(),
            target: None,
            cache_from: Vec::new(),
            cache_to: None,
            options: Vec::new(),
            secrets: Vec::new(),
            use_buildkit: true,
            docker_path: None,
            platform: None,
        }
    }

    #[test]
    fn build_args_basic() {
        // Legacy (classic) build: no `buildx`, no `--progress`, no `--load`.
        let opts = basic_opts();
        let args = build_command_args(&opts, false);
        assert_eq!(
            args,
            vec![
                "build",
                "-t",
                "myimage:latest",
                "-f",
                "/src/project/Dockerfile",
                "/src/project",
            ]
        );
    }

    #[test]
    fn build_args_with_buildx() {
        let opts = basic_opts();
        let args = build_command_args(&opts, true);
        assert_eq!(args[0], "buildx");
        assert_eq!(args[1], "build");
        assert_eq!(args[2], "--progress=plain");
        assert_eq!(args[3], "--load");
    }

    #[test]
    fn build_args_with_target() {
        let mut opts = basic_opts();
        opts.target = Some("builder".to_string());
        let args = build_command_args(&opts, false);
        assert!(args.contains(&"--target".to_string()));
        assert!(args.contains(&"builder".to_string()));
    }

    #[test]
    fn build_args_with_build_args() {
        let mut opts = basic_opts();
        opts.args
            .insert("NODE_VERSION".to_string(), "20".to_string());
        let args = build_command_args(&opts, false);
        assert!(args.contains(&"--build-arg".to_string()));
        assert!(args.contains(&"NODE_VERSION=20".to_string()));
    }

    #[test]
    fn build_args_with_cache_from() {
        let mut opts = basic_opts();
        opts.cache_from = vec!["myimage:cache".to_string()];
        let args = build_command_args(&opts, false);
        assert!(args.contains(&"--cache-from".to_string()));
        assert!(args.contains(&"myimage:cache".to_string()));
    }

    #[test]
    fn build_args_with_options_passthrough() {
        let mut opts = basic_opts();
        opts.options = vec!["--no-cache".to_string(), "--pull".to_string()];
        let args = build_command_args(&opts, false);
        assert!(args.contains(&"--no-cache".to_string()));
        assert!(args.contains(&"--pull".to_string()));
        // Context path is always last
        assert_eq!(args.last().unwrap(), "/src/project");
    }

    #[test]
    fn normalize_user_plain_username() {
        assert_eq!(normalize_user("vscode"), "vscode");
    }

    #[test]
    fn normalize_user_with_group() {
        assert_eq!(normalize_user("vscode:vscode"), "vscode");
    }

    #[test]
    fn normalize_user_uid_only() {
        assert_eq!(normalize_user("1000"), "1000");
    }

    #[test]
    fn normalize_user_uid_gid() {
        assert_eq!(normalize_user("1000:1000"), "1000");
    }

    #[test]
    fn normalize_user_empty_defaults_to_root() {
        assert_eq!(normalize_user(""), "root");
    }

    #[test]
    fn normalize_user_colon_only_defaults_to_root() {
        assert_eq!(normalize_user(":1000"), "root");
    }

    // -----------------------------------------------------------------------
    // normalize_user additional edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn normalize_user_multiple_colons() {
        // "user:group:extra" should return "user"
        assert_eq!(normalize_user("user:group:extra"), "user");
    }

    #[test]
    fn normalize_user_numeric_with_colon_and_extra() {
        assert_eq!(normalize_user("1000:1000:extra"), "1000");
    }

    #[test]
    fn normalize_user_whitespace() {
        // Whitespace is not trimmed -- returned as-is
        assert_eq!(normalize_user(" user "), " user ");
    }

    // -----------------------------------------------------------------------
    // build_command_args additional tests
    // -----------------------------------------------------------------------

    #[test]
    fn build_args_context_path_is_always_last() {
        let opts = basic_opts();
        let args = build_command_args(&opts, false);
        assert_eq!(args.last().unwrap(), "/src/project");
    }

    #[test]
    fn build_args_buildx_loads_image() {
        let opts = basic_opts();
        let args = build_command_args(&opts, true);
        assert!(args.contains(&"--load".to_string()));
    }

    #[test]
    fn build_args_without_buildx_no_load() {
        let opts = basic_opts();
        let args = build_command_args(&opts, false);
        assert!(!args.contains(&"--load".to_string()));
    }

    #[test]
    fn build_args_progress_plain_only_with_buildx() {
        // `--progress=plain` is buildx-only; the classic builder rejects it,
        // and the official CLI never emits it on the legacy path.
        let opts = basic_opts();
        let args_no_buildx = build_command_args(&opts, false);
        let args_buildx = build_command_args(&opts, true);
        assert!(!args_no_buildx.contains(&"--progress=plain".to_string()));
        assert!(args_buildx.contains(&"--progress=plain".to_string()));
    }

    #[test]
    fn build_args_dockerfile_joined_with_context() {
        let mut opts = basic_opts();
        opts.dockerfile = "docker/Dockerfile.dev".to_string();
        let args = build_command_args(&opts, false);
        assert!(
            args.contains(&"/src/project/docker/Dockerfile.dev".to_string()),
            "Dockerfile path should be joined with context: {args:?}"
        );
    }

    #[test]
    fn build_args_multiple_build_args() {
        let mut opts = basic_opts();
        opts.args
            .insert("NODE_VERSION".to_string(), "20".to_string());
        opts.args
            .insert("PYTHON_VERSION".to_string(), "3.11".to_string());
        let args = build_command_args(&opts, false);
        // Both build args should appear
        let build_arg_count = args.iter().filter(|a| *a == "--build-arg").count();
        assert_eq!(build_arg_count, 2);
    }

    #[test]
    fn build_args_multiple_cache_from() {
        let mut opts = basic_opts();
        opts.cache_from = vec!["cache1".to_string(), "cache2".to_string()];
        let args = build_command_args(&opts, false);
        let cache_count = args.iter().filter(|a| *a == "--cache-from").count();
        assert_eq!(cache_count, 2);
    }

    #[test]
    fn build_args_empty_options() {
        let opts = basic_opts();
        let args = build_command_args(&opts, false);
        // Minimal legacy build: ["build", "-t", name, "-f", path, context]
        assert_eq!(args.len(), 6);
    }

    #[test]
    fn build_args_with_secret_id_only() {
        let mut opts = basic_opts();
        opts.secrets = vec![cella_backend::BuildSecret {
            id: "mysecret".to_string(),
            src: None,
            env: None,
        }];
        let args = build_command_args(&opts, false);
        assert!(args.contains(&"--secret".to_string()));
        assert!(args.contains(&"id=mysecret".to_string()));
        // Context path is still last
        assert_eq!(args.last().unwrap(), "/src/project");
    }

    #[test]
    fn build_args_with_secret_src() {
        let mut opts = basic_opts();
        opts.secrets = vec![cella_backend::BuildSecret {
            id: "token".to_string(),
            src: Some(PathBuf::from("/run/secrets/token")),
            env: None,
        }];
        let args = build_command_args(&opts, false);
        let secret_idx = args.iter().position(|a| a == "--secret").unwrap();
        assert_eq!(args[secret_idx + 1], "id=token,src=/run/secrets/token");
    }

    #[test]
    fn build_args_with_secret_env() {
        let mut opts = basic_opts();
        opts.secrets = vec![cella_backend::BuildSecret {
            id: "token".to_string(),
            src: None,
            env: Some("MY_TOKEN".to_string()),
        }];
        let args = build_command_args(&opts, false);
        let secret_idx = args.iter().position(|a| a == "--secret").unwrap();
        assert_eq!(args[secret_idx + 1], "id=token,env=MY_TOKEN");
    }

    #[test]
    fn build_args_with_secret_src_and_env() {
        let mut opts = basic_opts();
        opts.secrets = vec![cella_backend::BuildSecret {
            id: "s".to_string(),
            src: Some(PathBuf::from("/tmp/s")),
            env: Some("S_VAR".to_string()),
        }];
        let args = build_command_args(&opts, false);
        let secret_idx = args.iter().position(|a| a == "--secret").unwrap();
        assert_eq!(args[secret_idx + 1], "id=s,src=/tmp/s,env=S_VAR");
    }

    #[test]
    fn build_args_with_multiple_secrets() {
        let mut opts = basic_opts();
        opts.secrets = vec![
            cella_backend::BuildSecret {
                id: "a".to_string(),
                src: None,
                env: None,
            },
            cella_backend::BuildSecret {
                id: "b".to_string(),
                src: Some(PathBuf::from("/tmp/b")),
                env: None,
            },
        ];
        let args = build_command_args(&opts, false);
        let secret_count = args.iter().filter(|a| *a == "--secret").count();
        assert_eq!(secret_count, 2);
        assert_eq!(args.last().unwrap(), "/src/project");
    }

    // -----------------------------------------------------------------------
    // --cache-to / BuildKit / inline-cache
    // -----------------------------------------------------------------------

    #[test]
    fn build_args_cache_to_buildx_registry_adds_inline_cache_arg() {
        let mut opts = basic_opts();
        opts.cache_to = Some("type=registry,ref=r".to_string());
        let args = build_command_args(&opts, true);
        let idx = args.iter().position(|a| a == "--cache-to").unwrap();
        assert_eq!(args[idx + 1], "type=registry,ref=r");
        assert!(args.contains(&"BUILDKIT_INLINE_CACHE=1".to_string()));
    }

    #[test]
    fn build_args_cache_to_inline_skips_inline_cache_arg() {
        let mut opts = basic_opts();
        opts.cache_to = Some("type=inline".to_string());
        let args = build_command_args(&opts, true);
        assert!(args.contains(&"type=inline".to_string()));
        assert!(!args.contains(&"BUILDKIT_INLINE_CACHE=1".to_string()));
    }

    #[test]
    fn build_args_cache_to_dropped_without_buildx() {
        // buildkit=never (use_buildx == false): cache-to is silently dropped.
        let mut opts = basic_opts();
        opts.cache_to = Some("type=registry,ref=r".to_string());
        let args = build_command_args(&opts, false);
        assert!(!args.contains(&"--cache-to".to_string()));
        assert!(!args.contains(&"BUILDKIT_INLINE_CACHE=1".to_string()));
    }

    #[test]
    fn build_args_buildkit_never_omits_buildx_subcommand() {
        // The classic-builder argument list never starts with `buildx`.
        let opts = basic_opts();
        let args = build_command_args(&opts, false);
        assert_eq!(args[0], "build");
        assert!(!args.contains(&"buildx".to_string()));
    }

    #[test]
    fn build_args_cache_from_present() {
        // The orchestrator appends CLI `--cache-from` into `cache_from`; this
        // verifies every entry is emitted in order on the build command.
        let mut opts = basic_opts();
        opts.cache_from = vec!["cli:1".to_string(), "cfg:1".to_string()];
        let args = build_command_args(&opts, true);
        let positions: Vec<usize> = args
            .iter()
            .enumerate()
            .filter(|(_, a)| *a == "--cache-from")
            .map(|(i, _)| i)
            .collect();
        assert_eq!(positions.len(), 2);
        assert_eq!(args[positions[0] + 1], "cli:1");
        assert_eq!(args[positions[1] + 1], "cfg:1");
    }

    #[test]
    fn build_args_with_platform() {
        let mut opts = basic_opts();
        opts.platform = Some("linux/amd64".to_string());
        let args = build_command_args(&opts, false);
        assert!(args.contains(&"--platform".to_string()));
        assert!(args.contains(&"linux/amd64".to_string()));
    }

    #[test]
    fn build_args_no_platform_omits_flag() {
        let opts = basic_opts();
        let args = build_command_args(&opts, false);
        assert!(!args.contains(&"--platform".to_string()));
    }

    #[test]
    fn build_args_platform_with_buildkit_correct_order() {
        // buildx path: ["buildx", "build", "--progress=plain", "--load",
        //               "--platform", "linux/amd64", "-t", …]
        // --platform must appear before -t and after --load.
        let mut opts = basic_opts();
        opts.platform = Some("linux/amd64".to_string());
        let args = build_command_args(&opts, true);
        assert!(args.contains(&"--platform".to_string()));
        assert!(args.contains(&"linux/amd64".to_string()));
        let platform_idx = args.iter().position(|a| a == "--platform").unwrap();
        let tag_idx = args.iter().position(|a| a == "-t").unwrap();
        let load_idx = args.iter().position(|a| a == "--load").unwrap();
        assert!(
            load_idx < platform_idx,
            "--platform should come after --load; args: {args:?}"
        );
        assert!(
            platform_idx < tag_idx,
            "--platform should come before -t; args: {args:?}"
        );
    }

    #[test]
    fn build_args_no_platform_none_omits_flag_buildkit() {
        // platform: None on the buildx path must not emit --platform at all.
        let opts = basic_opts();
        let args = build_command_args(&opts, true);
        assert!(
            !args.contains(&"--platform".to_string()),
            "platform:None must not emit --platform; args: {args:?}"
        );
    }

    #[test]
    fn is_cache_to_inline_matches() {
        assert!(is_cache_to_inline("type=inline"));
        assert!(is_cache_to_inline("type = inline"));
        assert!(is_cache_to_inline("TYPE=INLINE"));
        assert!(is_cache_to_inline("dest=x,type=inline,mode=max"));
        assert!(!is_cache_to_inline("type=registry,ref=r"));
        assert!(!is_cache_to_inline(""));
    }

    #[test]
    fn docker_binary_uses_override_path() {
        // A nonexistent docker path surfaces as not-found rather than silently
        // falling back to the default `docker`.
        let err = docker_binary(Some("/nonexistent/docker-binary-xyz")).unwrap_err();
        assert!(matches!(err, CellaDockerError::DockerCliNotFound { .. }));
    }

    #[test]
    fn has_buildx_false_for_missing_binary() {
        assert!(!has_buildx(Some("/nonexistent/docker-binary-xyz")));
    }
}
