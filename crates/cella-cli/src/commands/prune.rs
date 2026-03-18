use clap::Args;

/// Remove stale worktrees and their associated containers.
#[derive(Args)]
pub struct PruneArgs {
    /// Remove without confirmation.
    #[arg(long)]
    force: bool,
}

impl PruneArgs {
    pub async fn execute(self) -> Result<(), Box<dyn std::error::Error>> {
        eprintln!("cella prune: not yet implemented");
        Err("not yet implemented".into())
    }
}
