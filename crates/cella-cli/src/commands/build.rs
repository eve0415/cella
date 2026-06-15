use std::path::PathBuf;

use clap::Args;
use tracing::{info, warn};

use super::{BuildKitMode, ComposePullPolicy, ImagePullPolicy, LogFormat, LogLevel};

use cella_backend::{BuildSecret, container_name};
use cella_config::devcontainer::resolve;
use cella_orchestrator::image::EnsureImageInput;

/// Build the dev container image without starting it.
#[derive(Args)]
pub struct BuildArgs {
    #[command(flatten)]
    pub verbose: super::VerboseArgs,

    /// Do not use cache when building the image.
    #[arg(long)]
    no_cache: bool,

    /// Image pull policy.
    #[arg(long, value_enum)]
    pull: Option<ImagePullPolicy>,

    /// Explicit workspace folder path (defaults to current directory).
    #[arg(long)]
    workspace_folder: Option<PathBuf>,

    /// Path to devcontainer.json (overrides auto-discovery).
    #[arg(long)]
    config: Option<PathBuf>,

    /// Additional features to apply, as a JSON object matching the
    /// devcontainer.json `features` section. Merged into the resolved config
    /// before the build (applies to both compose and single-container).
    #[arg(long = "additional-features")]
    additional_features: Option<String>,

    #[command(flatten)]
    backend: crate::backend::BackendArgs,

    /// `BuildKit` secret to pass to the build (format: `id=X[,src=Y][,env=Z]`).
    /// Can be specified multiple times.
    #[arg(long = "secret")]
    secrets: Vec<String>,

    /// Name(s) to tag the built/resolved image with (repeatable). When set,
    /// these are the names reported in the build result (matching the official
    /// `devcontainer build --image-name`).
    #[arg(long = "image-name")]
    image_name: Vec<String>,

    /// Docker Compose profile(s) to activate (repeatable).
    #[arg(long = "profile")]
    profile: Vec<String>,

    /// Extra env-file(s) to pass to Docker Compose (repeatable).
    #[arg(long = "env-file")]
    env_file: Vec<PathBuf>,

    /// Pull policy for Docker Compose services.
    #[arg(long = "pull-policy", value_enum)]
    pull_policy: Option<ComposePullPolicy>,

    /// Set target platform(s) for the build (e.g. `linux/amd64`). Passed to the
    /// underlying docker/buildx build. Not supported on the Docker Compose path.
    #[arg(long)]
    platform: Option<String>,

    /// Additional image(s) to use as a layer cache during the build (repeatable).
    /// Single-container path only (not supported on the Docker Compose path).
    #[arg(long = "cache-from")]
    cache_from: Vec<String>,

    /// Cache export destination for the build (`BuildKit` `--cache-to`).
    /// Single-container path only (not supported on the Docker Compose path).
    #[arg(long = "cache-to")]
    cache_to: Option<String>,

    /// buildx output spec, passed through to `docker buildx build --output`
    /// (e.g. `type=local,dest=./out`, `type=tar,dest=image.tar`). Overrides the
    /// default behavior of loading the built image into the local docker store.
    /// buildx-only (errors without `BuildKit`/buildx); single-container path
    /// only (not supported on the Docker Compose path).
    #[arg(long = "output")]
    output: Option<String>,

    /// Add metadata to the built image as a `key=value` image label
    /// (repeatable). The value may be empty (`key=`). On the single-container
    /// path it is emitted as `docker build --label key=value` (classic and
    /// buildx builders alike). On the Docker Compose path it is applied as the
    /// primary service's `build.labels`, so the label bakes into the built
    /// service image (matching official `devcontainer build --label`). A config
    /// with no build to attach to — a bare single-container `image:`, or a
    /// no-features image-only compose service — errors rather than silently
    /// ignoring the flag.
    #[arg(long = "label", value_parser = parse_build_label)]
    label: Vec<String>,

    /// Control whether `BuildKit` is used when building images.
    #[arg(long, value_enum, default_value = "auto")]
    buildkit: BuildKitMode,

    /// Path to the Docker CLI binary (used for image builds and compose).
    #[arg(long = "docker-path")]
    docker_path: Option<String>,

    /// Path to the Docker Compose CLI binary.
    #[arg(long = "docker-compose-path")]
    docker_compose_path: Option<String>,

    /// Log verbosity for build output.
    #[arg(long = "log-level", value_enum)]
    log_level: Option<LogLevel>,

    /// Log output format.
    #[arg(long = "log-format", value_enum, default_value = "text")]
    log_format: LogFormat,

    #[command(flatten)]
    lockfile: super::LockfileArgs,

    #[command(flatten)]
    deprecated_lockfile: super::DeprecatedLockfileArgs,

    /// Do not persist `customizations` from features into the
    /// `devcontainer.metadata` image label. Hidden parity flag matching the
    /// official devcontainer CLI.
    #[arg(long = "skip-persisting-customizations-from-features", hide = true)]
    skip_persisting_customizations_from_features: bool,

    #[command(flatten)]
    compat: BuildCompatArgs,
}

/// Compatibility/diagnostic flags accepted for `devcontainer build` parity.
///
/// All no-ops in cella, split into their own struct so `BuildArgs` stays under
/// `clippy::struct_excessive_bools`. Source: devcontainers/cli `buildOptions`
/// (`devContainersSpecCLI.ts`, lines 525, 542, 548).
#[derive(Args)]
struct BuildCompatArgs {
    /// Host directory persisted across sessions (compatibility no-op). cella
    /// manages its own data dirs; accepted for drop-in parity with the official
    /// `devcontainer build --user-data-folder`.
    #[arg(long = "user-data-folder")]
    user_data_folder: Option<PathBuf>,

    /// Temporary option for testing; cella performs no feature auto-mapping on
    /// the build path (compatibility no-op, hidden — matches the official
    /// `hidden: true`).
    #[arg(long = "skip-feature-auto-mapping", hide = true)]
    skip_feature_auto_mapping: bool,

    // `--omit-syntax-directive` is a true no-op in cella's build path. The
    // official CLI has two effects: (A) it parses the user's Dockerfile in JS
    // and strips any `# syntax=` directive (a moby/buildkit#4556 workaround),
    // and (B) it suppresses the `# syntax=` line it would otherwise prepend to
    // its generated feature-extension Dockerfile. cella does NEITHER: it never
    // parses Dockerfile content (`BuildOptions.dockerfile` is a filename passed
    // verbatim as `-f`; the engine reads any `# syntax=` natively), and
    // `cella_features::generate_dockerfile` emits no `# syntax=` line at all (it
    // opens with `ARG`/`FROM`). So cella already behaves as if this flag is
    // permanently on — there is nothing to suppress. Accepted-and-ignored for
    // drop-in parity (mirrors `up`'s `--omit-syntax-directive`).
    /// Omit Dockerfile syntax directives (compatibility no-op).
    #[arg(long = "omit-syntax-directive", hide = true)]
    omit_syntax_directive: bool,
}

impl BuildArgs {
    /// The `--log-level` value, seeded into the global tracing filter by main.rs
    /// before dispatch (mirrors `up`).
    pub const fn log_level(&self) -> Option<LogLevel> {
        self.log_level
    }

    /// The `--log-format` value (defaults to `Text`).
    pub const fn log_format(&self) -> LogFormat {
        self.log_format
    }

    pub async fn execute(
        self,
        progress: crate::progress::Progress,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        match self.run(progress).await {
            Ok(()) => Ok(()),
            // Mirror the official: a build failure prints the error envelope to
            // stdout (`{outcome:'error', …}`) and exits 1 — consistent with
            // up/set-up/run-user-commands rather than a bare stderr report.
            Err(e) => {
                println!("{}", render_error_envelope(&e.to_string()));
                std::process::exit(1);
            }
        }
    }

    async fn run(
        self,
        progress: crate::progress::Progress,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let cwd = super::resolve_workspace_folder(self.workspace_folder.as_deref())?;

        info!("Resolving devcontainer config...");
        let mut resolved = resolve::config(&cwd, self.config.as_deref())?;

        for w in &resolved.warnings {
            warn!("{}", w.message);
        }

        // Merge CLI --additional-features into the resolved config BEFORE the
        // compose/single-container split, so both paths build the added features
        // (the official CLI applies --additional-features to both).
        if let Some(ref additional) = self.additional_features {
            super::features::resolve::merge_additional_features(&mut resolved.config, additional)
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
        }

        let config = &resolved.config;
        let config_name = resolved.name();
        let fallback_name = container_name(&resolved.workspace_root, config_name);
        let secrets: Vec<BuildSecret> = self
            .secrets
            .iter()
            .map(|s| parse_build_secret(s))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;

        let client = self.backend.resolve_client().await?;
        client.ping().await?;
        let _title_guard = crate::title::push_for_workspace(
            client.as_ref(),
            &resolved.workspace_root,
            &fallback_name,
            None,
            None,
            "build",
        )
        .await;

        // Docker Compose path: delegate to orchestrator
        if config.get("dockerComposeFile").is_some() {
            return self
                .execute_compose(client.as_ref(), &resolved, &secrets, progress)
                .await;
        }

        self.execute_single_container(client.as_ref(), &resolved, &secrets, progress)
            .await
    }

    /// Reject or warn on build flags that don't apply to the Docker Compose path.
    ///
    /// Matches the official CLI, which errors on `--platform`/`--push`,
    /// `--output`, and `--cache-to` for compose. `--cache-from`/`--buildkit`
    /// only affect the single-container buildx path; the official CLI accepts
    /// them without error, so cella warns rather than failing (never silently
    /// ignores).
    fn reject_unsupported_compose_flags(
        &self,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if self.platform.is_some() {
            return Err("--platform or --push not supported.".into());
        }
        if self.output.is_some() {
            return Err("--output not supported.".into());
        }
        if self.cache_to.is_some() {
            return Err("--cache-to not supported.".into());
        }
        if !self.cache_from.is_empty() {
            warn!("--cache-from is ignored on the Docker Compose build path");
        }
        if matches!(self.buildkit, BuildKitMode::Never) {
            warn!("--buildkit is ignored on the Docker Compose build path");
        }
        Ok(())
    }

    /// Build the image on the Docker Compose path.
    ///
    /// Buildx-only flags are validated up front by
    /// [`reject_unsupported_compose_flags`].
    async fn execute_compose(
        &self,
        client: &dyn cella_backend::ContainerBackend,
        resolved: &resolve::ResolvedConfig,
        secrets: &[BuildSecret],
        progress: crate::progress::Progress,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.reject_unsupported_compose_flags()?;
        let (sender, renderer) = crate::progress::bridge(&progress);
        let build_cfg = cella_orchestrator::compose_build::ComposeBuildConfig {
            config: &resolved.config,
            config_path: &resolved.config_path,
            workspace_root: &resolved.workspace_root,
            profiles: self.profile.clone(),
            env_files: self.env_file.clone(),
            pull_policy: self.pull_policy.as_ref().map(|p| p.as_str().to_string()),
            secrets: secrets.to_vec(),
            docker_path: self.docker_path.clone(),
            docker_compose_path: self.docker_compose_path.clone(),
            lockfile_policy: super::derive_lockfile_policy(
                &self.lockfile,
                &self.deprecated_lockfile,
            ),
            // `--label` image labels: applied as the primary service's
            // `build.labels` (image labels on the built service image), the
            // compose equivalent of the single-container `docker build --label`.
            labels: self.label.clone(),
            omit_feature_customizations_from_metadata: self
                .skip_persisting_customizations_from_features,
        };
        let result = cella_orchestrator::compose_build::compose_build(client, &build_cfg, &sender)
            .await
            .map_err(|e| e.to_string());
        drop(sender);
        let _ = renderer.await;
        let result =
            result.map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;

        if !self.image_name.is_empty() {
            // `docker compose build` neither builds nor pulls an image-only
            // service, so the resolved image may be remote-only; `tag_image`
            // needs it present locally. Pull it if missing before tagging.
            if !client.image_exists(&result.image_name).await? {
                client.pull_image(&result.image_name).await?;
            }
            for name in &self.image_name {
                client.tag_image(&result.image_name, name).await?;
            }
        }

        print_result(&self.reported_names(&result.image_name), true);
        Ok(())
    }

    /// Build the image on the single-container (image / Dockerfile) path.
    async fn execute_single_container(
        &self,
        client: &dyn cella_backend::ContainerBackend,
        resolved: &resolve::ResolvedConfig,
        secrets: &[BuildSecret],
        progress: crate::progress::Progress,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let (sender, renderer) = crate::progress::bridge(&progress);
        let input = EnsureImageInput {
            client,
            config: &resolved.config,
            workspace_root: &resolved.workspace_root,
            config_name: resolved.name(),
            config_path: &resolved.config_path,
            no_cache: self.no_cache,
            pull_policy: self.pull.as_ref().map(ImagePullPolicy::as_str),
            secrets,
            build_tuning: cella_orchestrator::BuildTuning {
                docker_path: self.docker_path.as_deref(),
                use_buildkit: super::buildkit_enabled(self.buildkit),
                cli_cache_from: &self.cache_from,
                cache_to: self.cache_to.as_deref(),
                platform: self.platform.as_deref(),
            },
            // buildx `--output` export spec. The orchestrator routes it to the
            // single final build (base when there are no features, features
            // layer otherwise) so an export never starves a `FROM` of a base.
            output: self.output.as_deref(),
            // `--label` image labels. Routed to the same single final build as
            // `--output`; the orchestrator errors on a bare image:-no-features
            // config (no build to attach them to).
            labels: &self.label,
            // `--omit-config-remote-env-from-metadata` is wired on `up` only;
            // `cella build` keeps the full metadata label (up-only flag).
            omit_remote_env_from_metadata: false,
            omit_feature_customizations_from_metadata: self
                .skip_persisting_customizations_from_features,
            lockfile_policy: super::derive_lockfile_policy(
                &self.lockfile,
                &self.deprecated_lockfile,
            ),
            progress: &sender,
        };
        let result = cella_orchestrator::image::ensure_image(&input).await;
        drop(sender);
        let (img_name, _resolved_features, _image_details) = result?;
        let _ = renderer.await;

        // A `--output` export (e.g. type=local/tar) does NOT load the built
        // image into the docker store, so it can't be tagged afterward. The
        // official CLI feeds `-t` straight into the single buildx invocation,
        // where buildx ignores tags for non-loading outputs — so `--image-name`
        // is effectively a no-op alongside `--output`. Mirror that: skip the tag
        // step (which would otherwise fail with "no such image") and warn rather
        // than silently dropping it. The reported names still follow official,
        // which sets `imageNameResult = imageNames` regardless of `--output`.
        if self.output.is_some() {
            if !self.image_name.is_empty() {
                warn!(
                    "--image-name tags are not applied alongside --output: the export does not \
                     load an image into the docker store to tag"
                );
            }
        } else {
            for name in &self.image_name {
                client.tag_image(&img_name, name).await?;
            }
        }

        if let Some(container) = client.find_container(&resolved.workspace_root).await?
            && let Some(old_hash) = &container.config_hash
            && *old_hash != resolved.config_hash
        {
            warn!(
                "Config has changed since this container was created. Run `cella up --rebuild` to recreate with the updated config."
            );
        }

        print_result(&self.reported_names(&img_name), false);
        Ok(())
    }

    /// The image name(s) to report in the build result.
    ///
    /// When `--image-name` is set, the reported names are exactly those values
    /// (the official CLI sets `imageNameResult = imageNames`, *replacing* the
    /// built name). Otherwise the single built/resolved name is reported.
    fn reported_names(&self, built: &str) -> Vec<String> {
        if self.image_name.is_empty() {
            vec![built.to_string()]
        } else {
            self.image_name.clone()
        }
    }
}

/// JSON result emitted by `cella build` on stdout.
///
/// Mirrors the official `devcontainer build` contract — devcontainers/cli
/// `devContainersSpecCLI.ts` emits `JSON.stringify({ outcome: 'success',
/// imageName: string[] })`. Using a struct (not a `serde_json::Value` map) keeps
/// the official field order — `outcome` then `imageName` — stable in the emitted
/// string regardless of `serde_json` features.
#[derive(serde::Serialize)]
struct BuildResult {
    outcome: &'static str,
    #[serde(rename = "imageName")]
    image_name: Vec<String>,
}

/// Build the compact JSON result line for `cella build`.
///
/// Compact, single-line output (like the official CLI's `JSON.stringify`) so
/// consumers that read one JSON object per line keep working. `image_names`
/// lists every reported name: the built name, or all `--image-name` values when
/// that flag is set.
fn build_json_result(image_names: &[String]) -> String {
    serde_json::to_string(&BuildResult {
        outcome: "success",
        image_name: image_names.to_vec(),
    })
    .unwrap_or_default()
}

/// Render the `build` error envelope (stdout JSON), mirroring the official's
/// `{outcome:'error', message, description}` written on build failure.
fn render_error_envelope(message: &str) -> String {
    serde_json::to_string(&serde_json::json!({
        "outcome": "error",
        "message": message,
        "description": "An error occurred building the container.",
    }))
    .expect("BUG: error envelope is not serialisable")
}

/// Print the build result.
///
/// Always emits the machine-readable JSON result line to **stdout** (matching
/// the official `devcontainer build`, which unconditionally writes
/// `JSON.stringify(result)`), plus a friendly human summary to **stderr**. The
/// human line is a cella extra; keeping it on stderr means it never pollutes the
/// stdout JSON that scripts parse, so there is no `--output text|json` selector.
///
/// `image_names` holds every reported name (the built name, or all
/// `--image-name` values when set). The human summary lists them comma-separated.
fn print_result(image_names: &[String], compose: bool) {
    println!("{}", build_json_result(image_names));

    let joined = image_names.join(", ");
    match (compose, image_names.len() > 1) {
        (true, false) => eprintln!("Compose services built. Primary image: {joined}"),
        (true, true) => eprintln!("Compose services built. Tagged images: {joined}"),
        (false, false) => eprintln!("Image built: {joined}"),
        (false, true) => eprintln!("Image built and tagged: {joined}"),
    }
}

/// Parse a `--secret` CLI value into a [`BuildSecret`].
///
/// Expected format: `id=NAME[,src=PATH][,env=VAR]`.
pub fn parse_build_secret(s: &str) -> Result<BuildSecret, String> {
    let mut id = None;
    let mut src = None;
    let mut env = None;
    for part in s.split(',') {
        if let Some(val) = part.strip_prefix("id=") {
            id = Some(val.to_string());
        } else if let Some(val) = part.strip_prefix("src=") {
            src = Some(PathBuf::from(val));
        } else if let Some(val) = part.strip_prefix("env=") {
            env = Some(val.to_string());
        }
    }
    Ok(BuildSecret {
        id: id.ok_or("missing id= in --secret value")?,
        src,
        env,
    })
}

/// Validate a `--label` value (`key=value`). The key must be non-empty; the
/// value may be empty (`key=`), matching `docker build --label` (which accepts
/// `k=`). Mirrors [`crate::commands::parse_remote_env`]'s shape (non-empty key,
/// any value) rather than `parse_id_label` (which also requires a non-empty
/// value). Rejected at parse time so a malformed label never reaches the build.
pub fn parse_build_label(s: &str) -> Result<String, String> {
    match s.split_once('=') {
        Some((k, _)) if !k.is_empty() => Ok(s.to_string()),
        _ => Err("label must match <key>=<value> (key required; value may be empty)".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_secret_id_only() {
        let secret = parse_build_secret("id=mysecret").unwrap();
        assert_eq!(secret.id, "mysecret");
        assert!(secret.src.is_none());
        assert!(secret.env.is_none());
    }

    #[test]
    fn parse_secret_id_and_src() {
        let secret = parse_build_secret("id=mysecret,src=/run/secrets/token").unwrap();
        assert_eq!(secret.id, "mysecret");
        assert_eq!(secret.src.unwrap(), PathBuf::from("/run/secrets/token"));
        assert!(secret.env.is_none());
    }

    #[test]
    fn parse_secret_id_and_env() {
        let secret = parse_build_secret("id=mysecret,env=MY_TOKEN").unwrap();
        assert_eq!(secret.id, "mysecret");
        assert!(secret.src.is_none());
        assert_eq!(secret.env.unwrap(), "MY_TOKEN");
    }

    #[test]
    fn parse_secret_all_fields() {
        let secret = parse_build_secret("id=mysecret,src=/tmp/secret.txt,env=SECRET_VAR").unwrap();
        assert_eq!(secret.id, "mysecret");
        assert_eq!(secret.src.unwrap(), PathBuf::from("/tmp/secret.txt"));
        assert_eq!(secret.env.unwrap(), "SECRET_VAR");
    }

    #[test]
    fn parse_secret_missing_id_fails() {
        let result = parse_build_secret("src=/tmp/secret.txt");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("missing id="));
    }

    #[test]
    fn parse_secret_empty_string_fails() {
        let result = parse_build_secret("");
        assert!(result.is_err());
    }

    #[test]
    fn parse_secret_unknown_keys_ignored() {
        let secret = parse_build_secret("id=mysecret,foo=bar").unwrap();
        assert_eq!(secret.id, "mysecret");
    }

    use clap::Parser;

    /// Minimal CLI wrapper to parse `BuildArgs` in isolation.
    #[derive(Parser)]
    struct TestCli {
        #[command(flatten)]
        args: BuildArgs,
    }

    fn parse_build(extra: &[&str]) -> BuildArgs {
        let mut argv = vec!["build"];
        argv.extend_from_slice(extra);
        TestCli::try_parse_from(argv).unwrap().args
    }

    #[test]
    fn buildkit_default_is_auto_enabled() {
        // No --buildkit flag → auto → BuildKit enabled.
        let args = parse_build(&[]);
        assert!(super::super::buildkit_enabled(args.buildkit));
    }

    #[test]
    fn buildkit_never_disables() {
        // `--buildkit never` maps to use_buildkit == false in BuildTuning.
        let args = parse_build(&["--buildkit", "never"]);
        assert!(!super::super::buildkit_enabled(args.buildkit));
    }

    #[test]
    fn buildkit_auto_enables() {
        let args = parse_build(&["--buildkit", "auto"]);
        assert!(super::super::buildkit_enabled(args.buildkit));
    }

    #[test]
    fn cache_from_is_repeatable() {
        let args = parse_build(&["--cache-from", "a:1", "--cache-from", "b:2"]);
        assert_eq!(args.cache_from, vec!["a:1".to_string(), "b:2".to_string()]);
    }

    #[test]
    fn cache_to_and_docker_paths_parse() {
        let args = parse_build(&[
            "--cache-to",
            "type=inline",
            "--docker-path",
            "/usr/bin/docker",
            "--docker-compose-path",
            "/usr/bin/docker-compose",
        ]);
        assert_eq!(args.cache_to.as_deref(), Some("type=inline"));
        assert_eq!(args.docker_path.as_deref(), Some("/usr/bin/docker"));
        assert_eq!(
            args.docker_compose_path.as_deref(),
            Some("/usr/bin/docker-compose")
        );
    }

    #[test]
    fn compose_rejects_platform_and_cache_to_but_warns_on_others() {
        // Official errors on --platform/--push and --cache-to for compose.
        assert!(
            parse_build(&["--platform", "linux/amd64"])
                .reject_unsupported_compose_flags()
                .is_err()
        );
        assert!(
            parse_build(&["--cache-to", "type=inline"])
                .reject_unsupported_compose_flags()
                .is_err()
        );
        // --cache-from / --buildkit are accepted on compose (warn, not error),
        // matching the official CLI which doesn't reject them.
        assert!(
            parse_build(&["--cache-from", "x"])
                .reject_unsupported_compose_flags()
                .is_ok()
        );
        assert!(
            parse_build(&["--buildkit", "never"])
                .reject_unsupported_compose_flags()
                .is_ok()
        );
        assert!(parse_build(&[]).reject_unsupported_compose_flags().is_ok());
    }

    // ── --label parsing / threading ─────────────────────────────────

    #[test]
    fn label_is_repeatable() {
        let args = parse_build(&["--label", "a=1", "--label", "b=2"]);
        assert_eq!(args.label, vec!["a=1".to_string(), "b=2".to_string()]);
    }

    #[test]
    fn label_defaults_to_empty() {
        assert!(parse_build(&[]).label.is_empty());
    }

    #[test]
    fn label_accepts_empty_value() {
        // `docker build --label k=` is valid (label with an empty value), so the
        // parser must accept it — unlike `--id-label`, which requires a value.
        let args = parse_build(&["--label", "k="]);
        assert_eq!(args.label, vec!["k=".to_string()]);
    }

    #[test]
    fn label_rejects_missing_equals() {
        // No `=` at all → rejected at parse time.
        assert!(TestCli::try_parse_from(["build", "--label", "novalue"]).is_err());
    }

    #[test]
    fn label_rejects_empty_key() {
        // `=value` has an empty key → rejected at parse time.
        assert!(TestCli::try_parse_from(["build", "--label", "=value"]).is_err());
    }

    #[test]
    fn parse_build_label_directly() {
        // Key required; value optional (may be empty).
        assert_eq!(parse_build_label("a=1").unwrap(), "a=1");
        assert_eq!(parse_build_label("a=").unwrap(), "a=");
        // A value with an `=` keeps the whole string (only the first `=` splits).
        assert_eq!(parse_build_label("a=b=c").unwrap(), "a=b=c");
        assert!(parse_build_label("novalue").is_err());
        assert!(parse_build_label("=value").is_err());
        assert!(parse_build_label("").is_err());
    }

    #[test]
    fn compose_accepts_label() {
        // The compose path now supports `--label` (applied as the primary
        // service's `build.labels`), so it must NOT be rejected by the flag gate.
        // The image-only-no-build boundary is enforced later, in `compose_build`.
        assert!(
            parse_build(&["--label", "a=1"])
                .reject_unsupported_compose_flags()
                .is_ok(),
            "compose must accept --label"
        );
    }

    #[test]
    fn output_buildx_spec_parses_and_is_captured() {
        // `--output` is a free-form buildx spec passed through to the build.
        let args = parse_build(&["--output", "type=local,dest=./out"]);
        assert_eq!(args.output.as_deref(), Some("type=local,dest=./out"));
    }

    #[test]
    fn output_defaults_to_none() {
        assert!(parse_build(&[]).output.is_none());
    }

    #[test]
    fn compose_rejects_output() {
        // Official `doBuild` rejects `--output` for the compose path with the
        // exact message "--output not supported." cella mirrors it.
        let err = parse_build(&["--output", "type=local,dest=./out"])
            .reject_unsupported_compose_flags()
            .expect_err("compose must reject --output");
        assert_eq!(err.to_string(), "--output not supported.");
    }

    #[test]
    fn output_with_image_name_still_reports_image_names() {
        // `--output` and `--image-name` can be set together. The tag step is
        // skipped at runtime (a non-loading export can't be tagged), but the
        // reported names still follow official, which sets
        // `imageNameResult = imageNames` regardless of `--output`.
        let args = parse_build(&[
            "--output",
            "type=local,dest=./out",
            "--image-name",
            "x:1",
            "--image-name",
            "y:2",
        ]);
        assert_eq!(args.output.as_deref(), Some("type=local,dest=./out"));
        assert_eq!(
            args.reported_names("built:latest"),
            vec!["x:1".to_string(), "y:2".to_string()]
        );
    }

    #[test]
    fn additional_features_parses_and_merges_into_config() {
        let args = parse_build(&[
            "--additional-features",
            r#"{"ghcr.io/x/y:1":{"version":"1"}}"#,
        ]);
        let json = args
            .additional_features
            .as_deref()
            .expect("--additional-features captured");
        let mut config = serde_json::json!({ "image": "ubuntu" });
        super::super::features::resolve::merge_additional_features(&mut config, json)
            .expect("merge succeeds");
        assert_eq!(config["features"]["ghcr.io/x/y:1"]["version"], "1");
    }

    #[test]
    fn log_level_and_format_accessors_return_parsed_values() {
        let args = parse_build(&["--log-level", "debug", "--log-format", "json"]);
        assert!(matches!(args.log_level(), Some(LogLevel::Debug)));
        assert!(matches!(args.log_format(), LogFormat::Json));
    }

    #[test]
    fn log_level_defaults_to_none_and_format_to_text() {
        let args = parse_build(&[]);
        assert!(args.log_level().is_none());
        assert!(matches!(args.log_format(), LogFormat::Text));
    }

    // ── lockfile policy derivation ──────────────────────────────────

    fn build_lockfile_policy(extra: &[&str]) -> cella_features::LockfilePolicy {
        let args = parse_build(extra);
        super::super::derive_lockfile_policy(&args.lockfile, &args.deprecated_lockfile)
    }

    #[test]
    fn lockfile_default_is_update() {
        assert_eq!(
            build_lockfile_policy(&[]),
            cella_features::LockfilePolicy::Update
        );
    }

    #[test]
    fn no_lockfile_maps_to_no_lockfile() {
        assert_eq!(
            build_lockfile_policy(&["--no-lockfile"]),
            cella_features::LockfilePolicy::NoLockfile
        );
    }

    #[test]
    fn frozen_lockfile_maps_to_frozen() {
        assert_eq!(
            build_lockfile_policy(&["--frozen-lockfile"]),
            cella_features::LockfilePolicy::Frozen
        );
    }

    #[test]
    fn experimental_frozen_lockfile_maps_to_frozen() {
        // Hidden deprecated alias behaves like --frozen-lockfile (matches up).
        assert_eq!(
            build_lockfile_policy(&["--experimental-frozen-lockfile"]),
            cella_features::LockfilePolicy::Frozen
        );
    }

    #[test]
    fn experimental_lockfile_is_noop_update() {
        // Deprecated; lockfile is written by default, so policy stays Update.
        assert_eq!(
            build_lockfile_policy(&["--experimental-lockfile"]),
            cella_features::LockfilePolicy::Update
        );
    }

    #[test]
    fn no_lockfile_conflicts_with_frozen() {
        let result = TestCli::try_parse_from(["build", "--no-lockfile", "--frozen-lockfile"]);
        assert!(result.is_err());
    }

    #[test]
    fn no_lockfile_conflicts_with_experimental_frozen() {
        let result =
            TestCli::try_parse_from(["build", "--no-lockfile", "--experimental-frozen-lockfile"]);
        assert!(result.is_err());
    }

    #[test]
    fn error_envelope_shape() {
        // Build failures emit `{outcome:'error', message, description}` to
        // stdout, mirroring the official and matching the sibling commands.
        let parsed: serde_json::Value =
            serde_json::from_str(&render_error_envelope("boom")).expect("valid JSON");
        assert_eq!(parsed["outcome"], "error");
        assert_eq!(parsed["message"], "boom");
        assert_eq!(
            parsed["description"],
            "An error occurred building the container."
        );
    }

    #[test]
    fn build_json_result_matches_official_shape() {
        // Official `devcontainer build` emits compact single-line
        // `{"outcome":"success","imageName":[<name>]}` in that field order:
        // `outcome` is "success" (not "built"), `imageName` is an array, no extra
        // keys, no pretty indentation.
        assert_eq!(
            build_json_result(&["ghcr.io/acme/devcontainer:latest".to_string()]),
            r#"{"outcome":"success","imageName":["ghcr.io/acme/devcontainer:latest"]}"#
        );
    }

    #[test]
    fn build_json_result_lists_all_image_names() {
        // With multiple `--image-name`, every name appears in the imageName array
        // in order (official `imageNameResult = imageNames`).
        assert_eq!(
            build_json_result(&["one:1".to_string(), "two:2".to_string()]),
            r#"{"outcome":"success","imageName":["one:1","two:2"]}"#
        );
    }

    #[test]
    fn build_always_emits_json_result_no_format_flag() {
        // `cella build` has no `--output text|json` format selector — it always
        // emits the JSON result to stdout (matching the official `devcontainer
        // build`, which unconditionally writes `JSON.stringify(result)`). Verify
        // the result line is well-formed regardless of which other flags were
        // parsed, so nothing can gate it off.
        for extra in [
            vec![],
            vec!["--no-cache"],
            vec!["--log-format", "json"],
            vec!["--image-name", "x:1"],
        ] {
            let args = parse_build(&extra);
            let line = build_json_result(&args.reported_names("built:latest"));
            let parsed: serde_json::Value =
                serde_json::from_str(&line).expect("result line is valid JSON");
            assert_eq!(parsed["outcome"], "success", "for flags {extra:?}");
            assert!(
                parsed["imageName"].is_array(),
                "imageName must be an array for flags {extra:?}"
            );
        }
    }

    #[test]
    fn image_name_is_repeatable() {
        let args = parse_build(&["--image-name", "a:1", "--image-name", "b:2"]);
        assert_eq!(args.image_name, vec!["a:1".to_string(), "b:2".to_string()]);
    }

    #[test]
    fn image_name_defaults_to_empty() {
        assert!(parse_build(&[]).image_name.is_empty());
    }

    #[test]
    fn reported_names_uses_built_when_no_image_name() {
        // No --image-name → report the single built/resolved name.
        let args = parse_build(&[]);
        assert_eq!(
            args.reported_names("built:latest"),
            vec!["built:latest".to_string()]
        );
    }

    #[test]
    fn reported_names_replaces_with_image_names() {
        // --image-name replaces the built name with exactly the given values.
        let args = parse_build(&["--image-name", "x:1", "--image-name", "y:2"]);
        assert_eq!(
            args.reported_names("built:latest"),
            vec!["x:1".to_string(), "y:2".to_string()]
        );
    }

    #[test]
    fn skip_persisting_customizations_from_features_defaults_false() {
        let args = parse_build(&[]);
        assert!(!args.skip_persisting_customizations_from_features);
    }

    #[test]
    fn skip_persisting_customizations_from_features_parses() {
        let args = parse_build(&["--skip-persisting-customizations-from-features"]);
        assert!(args.skip_persisting_customizations_from_features);
    }

    // ── newly-added compat flags ────────────────────────────────────

    #[test]
    fn user_data_folder_parses() {
        let args = parse_build(&["--user-data-folder", "/persist"]);
        assert_eq!(
            args.compat.user_data_folder,
            Some(PathBuf::from("/persist"))
        );
    }

    #[test]
    fn skip_feature_auto_mapping_defaults_false_and_parses() {
        assert!(!parse_build(&[]).compat.skip_feature_auto_mapping);
        assert!(
            parse_build(&["--skip-feature-auto-mapping"])
                .compat
                .skip_feature_auto_mapping
        );
    }

    #[test]
    fn omit_syntax_directive_defaults_false_and_parses() {
        // No-op in cella (the generated feature Dockerfile emits no `# syntax=`
        // line and cella never rewrites a user Dockerfile), but it must parse so
        // `devcontainer build --omit-syntax-directive` is accepted as a drop-in.
        assert!(!parse_build(&[]).compat.omit_syntax_directive);
        assert!(
            parse_build(&["--omit-syntax-directive"])
                .compat
                .omit_syntax_directive
        );
    }

    // ── devcontainer-CLI flag parity ───────────────────────────────
    //
    // Source of truth: devcontainers/cli `src/spec-node/devContainersSpecCLI.ts`
    // `buildOptions` (lines 523-548). Every official long flag MUST be declared
    // so no official `devcontainer build` invocation errors with "unknown
    // argument" — EXCEPT `--push`/`--output` of the registry-write surface that
    // cella intentionally defers (`--push`) or already supports (`--output` is
    // present). This test pins the surface so it can't silently drift again.
    const OFFICIAL_BUILD_FLAGS: &[&str] = &[
        "user-data-folder",
        "docker-path",
        "docker-compose-path",
        "workspace-folder",
        "config",
        "log-level",
        "log-format",
        "no-cache",
        "image-name",
        "cache-from",
        "cache-to",
        "buildkit",
        "platform",
        "label",
        "output",
        "additional-features",
        "skip-feature-auto-mapping",
        "skip-persisting-customizations-from-features",
        "experimental-lockfile",
        "experimental-frozen-lockfile",
        "no-lockfile",
        "frozen-lockfile",
        "omit-syntax-directive",
    ];

    #[test]
    fn build_flag_parity() {
        use clap::CommandFactory;
        use std::collections::HashSet;
        let cli = crate::Cli::command();
        let cmd = cli
            .find_subcommand("build")
            .expect("`build` subcommand must exist");
        let longs: HashSet<&str> = cmd
            .get_arguments()
            .filter_map(clap::Arg::get_long)
            .collect();
        let missing: Vec<&&str> = OFFICIAL_BUILD_FLAGS
            .iter()
            .filter(|f| !longs.contains(**f))
            .collect();
        assert!(
            missing.is_empty(),
            "`build` is missing official flags: {missing:?}"
        );
    }

    #[test]
    fn build_accepts_all_new_compat_flags_together() {
        // A maximal official-style invocation including the three newly-added
        // flags must parse Ok.
        let args = parse_build(&[
            "--user-data-folder",
            "/persist",
            "--skip-feature-auto-mapping",
            "--omit-syntax-directive",
        ]);
        assert_eq!(
            args.compat.user_data_folder,
            Some(PathBuf::from("/persist"))
        );
        assert!(args.compat.skip_feature_auto_mapping);
        assert!(args.compat.omit_syntax_directive);
    }
}
