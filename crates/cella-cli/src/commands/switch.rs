use clap::Args;

/// Switch to a different worktree-backed branch.
#[derive(Args)]
pub struct SwitchArgs {
    /// Name of the branch to switch to.
    pub name: String,
}

impl SwitchArgs {
    pub fn execute(self) -> Result<(), Box<dyn std::error::Error>> {
        let _ = self;
        eprintln!("cella switch: not yet implemented");
        Err("not yet implemented".into())
    }
}
