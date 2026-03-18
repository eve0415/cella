use clap::Args;

/// Start a dev container for the current workspace.
#[derive(Args)]
pub struct UpArgs {
    /// Rebuild the container image before starting.
    #[arg(long)]
    rebuild: bool,
}

impl UpArgs {
    pub async fn execute(self) -> Result<(), Box<dyn std::error::Error>> {
        eprintln!("cella up: not yet implemented");
        Err("not yet implemented".into())
    }
}
