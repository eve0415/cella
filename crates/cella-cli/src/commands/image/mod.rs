//! `cella image` subcommands: inspect and update the base image pinned in a
//! devcontainer configuration.
//!
//! A command group with one subcommand today, so that `cella image pin`
//! (digest) or Dockerfile support can land later without renaming anything.

pub mod candidates;
pub mod jsonc_edit;
pub mod release;
pub mod update;

use clap::{Args, Subcommand};

use crate::progress::Progress;

/// Manage the devcontainer base image.
#[derive(Args)]
pub struct ImageArgs {
    #[command(subcommand)]
    pub command: ImageCommand,
}

/// Available image subcommands.
#[derive(Subcommand)]
pub enum ImageCommand {
    /// Check for and apply base image updates.
    Update(update::UpdateArgs),
}

impl ImageArgs {
    /// Dispatch to the active subcommand.
    ///
    /// # Errors
    ///
    /// Propagates the subcommand's error.
    pub async fn execute(
        self,
        _progress: Progress,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        match self.command {
            ImageCommand::Update(args) => args.execute().await,
        }
    }
}
