use clap::Args;

/// Spawn an AI agent sandbox.
#[derive(Args)]
pub struct SpawnArgs {
    /// Agent preset to use.
    #[arg(long)]
    pub preset: Option<String>,

    /// Branch or worktree to attach the agent to.
    #[arg(long)]
    pub branch: Option<String>,
}

impl SpawnArgs {
    pub async fn execute(self) -> Result<(), Box<dyn std::error::Error>> {
        eprintln!("cella spawn: not yet implemented");
        Err("not yet implemented".into())
    }
}
