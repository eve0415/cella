//! `cella features publish` — package and push features to an OCI registry.

use std::path::PathBuf;

use clap::Args;

use cella_features::publish::{PublishOptions, publish, results_to_json};

/// Publish one or more devcontainer features to an OCI registry.
#[derive(Args)]
pub struct PublishArgs {
    /// Path to a single feature directory or collection root (default: `.`).
    #[arg(default_value = ".")]
    pub target: PathBuf,

    /// OCI registry to push to.
    #[arg(short = 'r', long, default_value = "ghcr.io")]
    pub registry: String,

    /// Namespace within the registry, e.g. `owner/repo`.
    #[arg(short = 'n', long)]
    pub namespace: String,

    /// Log verbosity level.
    #[arg(long, value_enum, default_value_t = crate::commands::LogLevel::Info)]
    pub log_level: crate::commands::LogLevel,
}

impl PublishArgs {
    /// Execute the publish command.
    ///
    /// # Errors
    ///
    /// Returns an error if packaging or pushing fails.
    pub async fn execute(self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let opts = PublishOptions {
            target: self.target,
            registry: self.registry,
            namespace: self.namespace,
        };

        match publish(&opts).await {
            Ok(results) => {
                println!("{}", results_to_json(&results));
                Ok(())
            }
            Err(e) => {
                eprintln!("{e}");
                eprintln!("Failed to publish features");
                Err(Box::new(e))
            }
        }
    }
}
