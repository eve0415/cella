use std::path::PathBuf;

use clap::Args;
use serde_json::json;
use tracing::{info, warn};

use super::OutputFormat;

use cella_backend::BuildSecret;
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

    /// Image pull policy (e.g. "always", "missing", "never").
    #[arg(long)]
    pull: Option<String>,

    /// Explicit workspace folder path (defaults to current directory).
    #[arg(long)]
    workspace_folder: Option<PathBuf>,

    /// Path to devcontainer.json (overrides auto-discovery).
    #[arg(long)]
    config: Option<PathBuf>,

    #[command(flatten)]
    backend: crate::backend::BackendArgs,

    /// `BuildKit` secret to pass to the build (format: `id=X[,src=Y][,env=Z]`).
    /// Can be specified multiple times.
    #[arg(long = "secret")]
    secrets: Vec<String>,

    /// Output format.
    #[arg(long, value_enum, default_value = "text")]
    output: OutputFormat,

    /// Docker Compose profile(s) to activate (repeatable).
    #[arg(long = "profile")]
    profile: Vec<String>,

    /// Extra env-file(s) to pass to Docker Compose (repeatable).
    #[arg(long = "env-file")]
    env_file: Vec<PathBuf>,

    /// Pull policy for Docker Compose services (always, missing, never).
    #[arg(long = "pull-policy")]
    pull_policy: Option<String>,
}

impl BuildArgs {
    pub const fn is_text_output(&self) -> bool {
        matches!(self.output, OutputFormat::Text)
    }

    pub async fn execute(
        self,
        progress: crate::progress::Progress,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let cwd = super::resolve_workspace_folder(self.workspace_folder.as_deref())?;

        info!("Resolving devcontainer config...");
        let resolved = resolve::config(&cwd, self.config.as_deref())?;

        for w in &resolved.warnings {
            warn!("{}", w.message);
        }

        let config = &resolved.config;
        let config_name = config.get("name").and_then(|v| v.as_str());
        let secrets: Vec<BuildSecret> = self
            .secrets
            .iter()
            .map(|s| parse_build_secret(s))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

        let client = self.backend.resolve_client().await?;
        client.ping().await?;

        // Docker Compose path: delegate to orchestrator
        if config.get("dockerComposeFile").is_some() {
            let (sender, renderer) = crate::progress::bridge(&progress);
            let build_cfg = cella_orchestrator::compose_build::ComposeBuildConfig {
                config,
                config_path: &resolved.config_path,
                workspace_root: &resolved.workspace_root,
                profiles: self.profile.clone(),
                env_files: self.env_file.clone(),
                pull_policy: self.pull_policy.clone(),
                secrets: secrets.clone(),
            };
            let result = cella_orchestrator::compose_build::compose_build(
                client.as_ref(),
                &build_cfg,
                &sender,
            )
            .await
            .map_err(|e| e.to_string());
            drop(sender);
            let _ = renderer.await;
            let result = result.map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

            print_result(&self.output, &result.image_name, true);
            return Ok(());
        }

        // Non-compose path
        let (sender, renderer) = crate::progress::bridge(&progress);
        let input = EnsureImageInput {
            client: client.as_ref(),
            config,
            workspace_root: &resolved.workspace_root,
            config_name,
            config_path: &resolved.config_path,
            no_cache: self.no_cache,
            pull_policy: self.pull.as_deref(),
            secrets: &secrets,
            progress: &sender,
        };
        let result = cella_orchestrator::image::ensure_image(&input).await;
        drop(sender);
        let (img_name, _resolved_features, _image_details) = result?;
        let _ = renderer.await;

        if let Some(container) = client.find_container(&resolved.workspace_root).await?
            && let Some(old_hash) = &container.config_hash
            && *old_hash != resolved.config_hash
        {
            eprintln!(
                "\x1b[33mWARNING:\x1b[0m Config has changed since this container was created."
            );
            eprintln!("  Run `cella up --rebuild` to recreate with the updated config.");
        }

        print_result(&self.output, &img_name, false);
        Ok(())
    }
}

/// Print the build result in the requested output format.
fn print_result(output: &OutputFormat, image_name: &str, compose: bool) {
    match output {
        OutputFormat::Text => {
            if compose {
                eprintln!("Compose services built. Primary image: {image_name}");
            } else {
                eprintln!("Image built: {image_name}");
            }
        }
        OutputFormat::Json => {
            let mut map = json!({
                "outcome": "built",
                "imageName": image_name,
            });
            if compose {
                map["compose"] = json!(true);
            }
            println!("{}", serde_json::to_string_pretty(&map).unwrap_or_default());
        }
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
}
