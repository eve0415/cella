use clap::Args;

/// Remove stale worktrees and their associated containers.
#[derive(Args)]
pub struct PruneArgs {
    /// Remove without confirmation.
    #[arg(long)]
    force: bool,
}

impl PruneArgs {
    pub fn execute(self) -> Result<(), Box<dyn std::error::Error>> {
        let _ = self;
        eprintln!("cella prune: not yet implemented");
        Err("not yet implemented".into())
    }
}
