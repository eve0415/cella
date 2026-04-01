//! `cella features` subcommands: edit, list, and update features in
//! devcontainer configurations.

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
    /// List configured or available features.
    List(list::ListArgs),
}

impl FeaturesArgs {
    pub fn is_text_output(&self) -> bool {
        match &self.command {
            FeaturesCommand::List(args) => args.is_text_output(),
        }
    }

    pub async fn execute(self, _progress: Progress) -> Result<(), Box<dyn std::error::Error>> {
        match self.command {
            FeaturesCommand::List(args) => args.execute().await,
        }
    }
}
