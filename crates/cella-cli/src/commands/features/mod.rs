//! `cella features` subcommands: edit, generate-docs, info, list, package,
//! resolve-dependencies, and update features in devcontainer configurations.

pub mod edit;
pub mod generate_docs;
pub mod info;
pub mod jsonc_edit;
pub mod list;
pub mod package;
pub mod prompts;
pub mod resolve;
pub mod resolve_dependencies;
pub mod update;

use clap::{Args, Subcommand};

use crate::commands::LogLevel;
use crate::progress::Progress;

/// Manage devcontainer features.
#[derive(Args)]
pub struct FeaturesArgs {
    #[command(subcommand)]
    pub command: FeaturesCommand,
}

/// Available features subcommands.
#[derive(Subcommand)]
pub enum FeaturesCommand {
    /// Edit features in an existing devcontainer configuration.
    Edit(edit::EditArgs),
    /// Generate README.md documentation for each feature in a collection.
    GenerateDocs(generate_docs::GenerateDocsArgs),
    /// Show information about a feature (manifest, tags, dependencies).
    Info(info::InfoArgs),
    /// List configured or available features.
    List(list::ListArgs),
    /// Package local feature sources into distributable tarballs.
    Package(package::PackageArgs),
    /// Resolve feature dependencies and print the installation order.
    ResolveDependencies(resolve_dependencies::ResolveDependenciesArgs),
    /// Check for and apply feature version updates.
    Update(update::UpdateArgs),
}

impl FeaturesArgs {
    /// Return the active subcommand's `--log-level`, if it carries one.
    ///
    /// `generate-docs`, `info`, `package`, and `resolve-dependencies` expose
    /// `--log-level`.
    /// Read by [`super::Command::log_level`] so the global tracing filter is
    /// seeded before dispatch — the same pattern used by `up` and templates.
    pub const fn log_level(&self) -> Option<LogLevel> {
        match &self.command {
            FeaturesCommand::GenerateDocs(args) => Some(args.log_level),
            FeaturesCommand::Info(args) => Some(args.log_level),
            FeaturesCommand::Package(args) => Some(args.log_level),
            FeaturesCommand::ResolveDependencies(args) => Some(args.log_level),
            _ => None,
        }
    }

    pub async fn execute(
        self,
        _progress: Progress,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        match self.command {
            FeaturesCommand::Edit(args) => args.execute().await,
            FeaturesCommand::GenerateDocs(args) => args.execute(),
            FeaturesCommand::Info(args) => args.execute().await,
            FeaturesCommand::List(args) => args.execute().await,
            FeaturesCommand::Package(args) => args.execute(),
            FeaturesCommand::ResolveDependencies(args) => args.execute().await,
            FeaturesCommand::Update(args) => args.execute().await,
        }
    }
}
