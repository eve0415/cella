//! `cella features` subcommands: edit, list, package, and update features in
//! devcontainer configurations.

pub mod edit;
pub mod info;
pub mod jsonc_edit;
pub mod list;
pub mod package;
pub mod prompts;
pub mod resolve;
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
    /// Show information about a feature (manifest, tags, dependencies).
    Info(info::InfoArgs),
    /// List configured or available features.
    List(list::ListArgs),
    /// Package local feature sources into distributable tarballs.
    Package(package::PackageArgs),
    /// Check for and apply feature version updates.
    Update(update::UpdateArgs),
}

impl FeaturesArgs {
    /// Return the `--log-level` from the `info` subcommand, if active.
    ///
    /// Read by [`super::Command::log_level`] so the global tracing filter is
    /// seeded before dispatch — the same pattern used by `up` and templates.
    pub const fn log_level(&self) -> Option<LogLevel> {
        match &self.command {
            FeaturesCommand::Info(args) => Some(args.log_level),
            FeaturesCommand::Package(args) => Some(args.log_level),
            _ => None,
        }
    }

    pub async fn execute(
        self,
        _progress: Progress,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        match self.command {
            FeaturesCommand::Edit(args) => args.execute().await,
            FeaturesCommand::Info(args) => args.execute().await,
            FeaturesCommand::List(args) => args.execute().await,
            FeaturesCommand::Package(args) => args.execute(),
            FeaturesCommand::Update(args) => args.execute().await,
        }
    }
}
