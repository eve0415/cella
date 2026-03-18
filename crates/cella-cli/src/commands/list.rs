use clap::Args;

/// List all dev containers managed by cella.
#[derive(Args)]
pub struct ListArgs {
    /// Show only running containers.
    #[arg(long)]
    running: bool,
}

impl ListArgs {
    pub async fn execute(self) -> Result<(), Box<dyn std::error::Error>> {
        eprintln!("cella list: not yet implemented");
        Err("not yet implemented".into())
    }
}
