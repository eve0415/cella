use clap::Args;

/// Build the dev container image without starting it.
#[derive(Args)]
pub struct BuildArgs {
    /// Do not use cache when building the image.
    #[arg(long)]
    no_cache: bool,
}

impl BuildArgs {
    pub async fn execute(self) -> Result<(), Box<dyn std::error::Error>> {
        eprintln!("cella build: not yet implemented");
        Err("not yet implemented".into())
    }
}
