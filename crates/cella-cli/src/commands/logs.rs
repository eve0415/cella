use clap::Args;

/// View logs from the dev container.
#[derive(Args)]
pub struct LogsArgs {
    /// Follow log output.
    #[arg(short, long)]
    follow: bool,
}

impl LogsArgs {
    pub fn execute(self) -> Result<(), Box<dyn std::error::Error>> {
        let _ = self;
        eprintln!("cella logs: not yet implemented");
        Err("not yet implemented".into())
    }
}
