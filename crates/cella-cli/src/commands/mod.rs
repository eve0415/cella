use clap::Subcommand;

/// Top-level CLI commands.
#[derive(Subcommand)]
pub enum Command {}

impl Command {
    pub async fn execute(self) -> Result<(), Box<dyn std::error::Error>> {
        match self {}
    }
}
