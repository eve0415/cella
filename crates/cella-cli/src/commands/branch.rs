use clap::Args;

/// Create a new worktree-backed branch with its own dev container.
#[derive(Args)]
pub struct BranchArgs {
    /// Name for the new branch.
    pub name: String,

    /// Base ref to branch from.
    #[arg(long)]
    pub base: Option<String>,

    /// Template to use for the new branch's container.
    #[arg(long)]
    pub template: Option<String>,
}

impl BranchArgs {
    #[allow(clippy::unused_async)]
    pub async fn execute(self) -> Result<(), Box<dyn std::error::Error>> {
        eprintln!("cella branch: not yet implemented");
        Err("not yet implemented".into())
    }
}
