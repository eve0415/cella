use clap::Args;

/// Execute a command inside the running dev container.
#[derive(Args)]
pub struct ExecArgs {
    /// The command to execute.
    #[arg(trailing_var_arg = true, required = true)]
    command: Vec<String>,
}

impl ExecArgs {
    pub async fn execute(self) -> Result<(), Box<dyn std::error::Error>> {
        eprintln!("cella exec: not yet implemented");
        Err("not yet implemented".into())
    }
}
