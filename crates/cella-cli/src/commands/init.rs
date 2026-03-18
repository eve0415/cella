use clap::Args;

/// Initialize cella in the current repository.
#[derive(Args)]
pub struct InitArgs {
    /// Template to initialize from.
    #[arg(long)]
    pub template: Option<String>,
}

impl InitArgs {
    pub async fn execute(self) -> Result<(), Box<dyn std::error::Error>> {
        eprintln!("cella init: not yet implemented");
        Err("not yet implemented".into())
    }
}
