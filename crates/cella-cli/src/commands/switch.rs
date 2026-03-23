use clap::Args;

/// Switch to a different worktree-backed branch.
#[derive(Args)]
pub struct SwitchArgs {
    /// Name of the branch to switch to.
    pub name: String,
}

impl SwitchArgs {
    #[allow(clippy::unused_async)]
    pub async fn execute(self) -> Result<(), Box<dyn std::error::Error>> {
        eprintln!("cella switch: not yet implemented");
        Err("not yet implemented".into())
    }
}
