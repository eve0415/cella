use clap::{Args, Subcommand};

/// View and manage cella configuration.
#[derive(Args)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub command: ConfigCommand,
}

#[derive(Subcommand)]
pub enum ConfigCommand {
    /// Show the resolved configuration.
    Show,
    /// Edit global cella settings.
    Global,
    /// Manage dotfile symlinks.
    Dotfiles,
    /// Configure agent presets.
    Agent,
}

impl ConfigArgs {
    pub async fn execute(self) -> Result<(), Box<dyn std::error::Error>> {
        match self.command {
            ConfigCommand::Show
            | ConfigCommand::Global
            | ConfigCommand::Dotfiles
            | ConfigCommand::Agent => {
                eprintln!("cella config: not yet implemented");
                Err("not yet implemented".into())
            }
        }
    }
}
