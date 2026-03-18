use clap::Args;

/// Open a shell inside the running dev container.
#[derive(Args)]
pub struct ShellArgs {
    /// Shell to use (e.g., bash, zsh, fish).
    #[arg(short, long)]
    shell: Option<String>,
}

impl ShellArgs {
    pub async fn execute(self) -> Result<(), Box<dyn std::error::Error>> {
        eprintln!("cella shell: not yet implemented");
        Err("not yet implemented".into())
    }
}
