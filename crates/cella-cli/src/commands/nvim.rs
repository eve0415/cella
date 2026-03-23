use clap::Args;

/// Open neovim connected to the dev container.
#[derive(Args)]
pub struct NvimArgs {
    /// File to open.
    pub file: Option<String>,
}

impl NvimArgs {
    pub fn execute(self) -> Result<(), Box<dyn std::error::Error>> {
        let _ = self;
        eprintln!("cella nvim: not yet implemented");
        Err("not yet implemented".into())
    }
}
