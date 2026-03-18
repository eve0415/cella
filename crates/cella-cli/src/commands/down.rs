use clap::Args;

/// Stop and remove the dev container for the current workspace.
#[derive(Args)]
pub struct DownArgs {
    /// Remove associated volumes.
    #[arg(long)]
    volumes: bool,
}

impl DownArgs {
    pub async fn execute(self) -> Result<(), Box<dyn std::error::Error>> {
        eprintln!("cella down: not yet implemented");
        Err("not yet implemented".into())
    }
}
