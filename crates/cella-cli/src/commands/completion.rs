use clap::{Args, CommandFactory, ValueEnum};
use clap_complete::{Shell, generate};

/// Generate shell completion scripts for cella.
///
/// Output the completion script to stdout. Pipe it to the appropriate
/// location for your shell:
///
///   cella completion bash > ~/.local/share/bash-completion/completions/cella
///   cella completion zsh > ~/.zfunc/_cella
///   cella completion fish > ~/.config/fish/completions/cella.fish
#[derive(Args)]
pub struct CompletionArgs {
    /// Shell to generate completion for.
    #[arg(value_enum)]
    shell: CompletionShell,
}

#[derive(Clone, ValueEnum)]
enum CompletionShell {
    Bash,
    Zsh,
    Fish,
}

impl CompletionArgs {
    pub fn execute(&self) {
        let shell = match self.shell {
            CompletionShell::Bash => Shell::Bash,
            CompletionShell::Zsh => Shell::Zsh,
            CompletionShell::Fish => Shell::Fish,
        };
        let mut cmd = crate::Cli::command();
        generate(shell, &mut cmd, "cella", &mut std::io::stdout());
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;
    use clap_complete::{Shell, generate};

    #[test]
    fn bash_completion_output() {
        let mut buf = Vec::new();
        let mut cmd = crate::Cli::command();
        generate(Shell::Bash, &mut cmd, "cella", &mut buf);
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("cella"));
        assert!(output.contains("switch"));
    }

    #[test]
    fn zsh_completion_output() {
        let mut buf = Vec::new();
        let mut cmd = crate::Cli::command();
        generate(Shell::Zsh, &mut cmd, "cella", &mut buf);
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("cella"));
    }

    #[test]
    fn fish_completion_output() {
        let mut buf = Vec::new();
        let mut cmd = crate::Cli::command();
        generate(Shell::Fish, &mut cmd, "cella", &mut buf);
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("cella"));
    }
}
