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
    pub fn execute(self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        match self.command {
            TemplateCommand::New { .. } | TemplateCommand::List | TemplateCommand::Edit { .. } => {
                eprintln!("cella template: not yet implemented");
                Err("not yet implemented".into())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_commands_return_not_implemented() {
        let commands = [
            TemplateCommand::New {
                name: "rust".to_string(),
            },
            TemplateCommand::List,
            TemplateCommand::Edit {
                name: "node".to_string(),
            },
        ];

        for command in commands {
            let result = TemplateArgs { command }.execute();
            assert!(result.is_err());
            assert_eq!(result.unwrap_err().to_string(), "not yet implemented");
        }
    }
}
