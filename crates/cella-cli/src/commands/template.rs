use clap::{Args, Subcommand};

/// Manage dev container templates.
#[derive(Args)]
pub struct TemplateArgs {
    #[command(subcommand)]
    pub command: TemplateCommand,
}

#[derive(Subcommand)]
pub enum TemplateCommand {
    /// Create a new template.
    New {
        /// Name for the template.
        name: String,
    },
    /// List available templates.
    List,
    /// Edit an existing template.
    Edit {
        /// Name of the template to edit.
        name: String,
    },
}

impl TemplateArgs {
    pub async fn execute(self) -> Result<(), Box<dyn std::error::Error>> {
        match self.command {
            TemplateCommand::New { .. }
            | TemplateCommand::List
            | TemplateCommand::Edit { .. } => {
                eprintln!("cella template: not yet implemented");
                Err("not yet implemented".into())
            }
        }
    }
}
