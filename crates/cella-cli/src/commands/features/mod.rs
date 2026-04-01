//! `cella features` subcommands: edit, list, and update features in
//! devcontainer configurations.

pub mod edit;
pub mod jsonc_edit;
pub mod list;
pub mod prompts;
pub mod resolve;

use clap::{Args, Subcommand};

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
    /// List configured or available features.
    List(list::ListArgs),
}

impl FeaturesArgs {
    pub async fn execute(self, _progress: Progress) -> Result<(), Box<dyn std::error::Error>> {
        match self.command {
            FeaturesCommand::Edit(args) => args.execute().await,
            FeaturesCommand::List(args) => args.execute().await,
        }
    }
}
