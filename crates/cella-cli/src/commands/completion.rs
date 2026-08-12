use clap::{Args, CommandFactory as _};

/// Environment variable that switches cella into completion mode.
///
/// `clap_complete`'s `CompleteEnv` reads this at startup, and the same name is
/// baked into the registration hook, which re-invokes cella with it set. Both
/// sides must agree, so `main` and this module share one constant.
///
/// Deliberately not `clap_complete`'s default `COMPLETE`: with that name, a
/// user who exports it for another clap-based tool turns *every* cella
/// invocation into "print a hook and exit successfully" — `cella up` would
/// silently not run.
pub const COMPLETE_VAR: &str = "CELLA_COMPLETE";

/// Print the shell hook that enables cella's completions.
///
/// The user-facing prose lives on the `Completion` variant in `commands/mod.rs`
/// — clap takes a subcommand's `about`/`long_about` from the enum variant's doc
/// comment, so a doc comment here would never reach `cella completion --help`.
#[derive(Args)]
pub struct CompletionArgs {
    /// Shell to print the completion hook for.
    #[arg(value_parser = shell_names())]
    shell: String,
}

/// Accepted `<SHELL>` values, read from `clap_complete`'s built-in list so a
/// newly supported shell needs no change here.
fn shell_names() -> clap::builder::PossibleValuesParser {
    clap::builder::PossibleValuesParser::new(
        clap_complete::env::Shells::builtins()
            .names()
            .collect::<Vec<_>>(),
    )
}

impl CompletionArgs {
    pub fn execute(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        use std::io::Write as _;

        let mut out = std::io::stdout().lock();
        self.write_hook(&mut out)?;
        out.flush()?;
        Ok(())
    }

    /// Render the registration hook for the selected shell.
    fn write_hook(
        &self,
        out: &mut dyn std::io::Write,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // `Shells::completer` returns a borrow tied to the receiver, and
        // `&dyn EnvCompleter` is not `Sync` — so this can be neither a `const`
        // nor a `static`. It has to be a local binding.
        let shells = clap_complete::env::Shells::builtins();
        let shell = shells
            .completer(&self.shell)
            .ok_or_else(|| format!("unknown shell `{}`", self.shell))?;

        // Mirror `CompleteEnv`'s own defaults so this subcommand and
        // `CELLA_COMPLETE=<shell> cella` emit byte-identical hooks.
        let cmd = crate::Cli::command();
        let name = cmd.get_name();
        shell.write_registration(COMPLETE_VAR, name, name, &completer_path(), out)?;
        Ok(())
    }
}

/// The command the generated hook re-invokes at completion time.
///
/// Reproduces `CompleteEnv`'s default: argv[0], absolutized against the
/// current directory when it has more than one component
/// (`./target/debug/cella`), left alone when it is a bare name resolved
/// through `PATH` (`cella`).
fn completer_path() -> String {
    let path =
        std::path::PathBuf::from(std::env::args_os().next().unwrap_or_else(|| "cella".into()));
    if path.components().count() > 1
        && let Ok(current_dir) = std::env::current_dir()
    {
        return current_dir.join(path).to_string_lossy().into_owned();
    }
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory as _, Parser as _};

    use super::{COMPLETE_VAR, CompletionArgs};

    fn hook_for(shell: &str) -> String {
        let args = CompletionArgs {
            shell: shell.to_owned(),
        };
        let mut buf = Vec::new();
        args.write_hook(&mut buf).expect("hook must render");
        String::from_utf8(buf).expect("hook must be UTF-8")
    }

    /// Every builtin shell must produce a hook that registers `cella` and sets
    /// the same variable `main` hands to `CompleteEnv` — the only way to check
    /// the two ends of the round-trip against each other.
    #[test]
    fn every_builtin_shell_emits_a_hook() {
        for name in clap_complete::env::Shells::builtins().names() {
            let hook = hook_for(name);
            assert!(
                hook.contains("cella"),
                "{name} hook must register `cella`:\n{hook}"
            );
            assert!(
                hook.contains(COMPLETE_VAR),
                "{name} hook must set {COMPLETE_VAR}:\n{hook}"
            );
        }
    }

    /// The dynamic hook must not inline the subcommand list — that would mean
    /// the ahead-of-time script came back.
    #[test]
    fn hook_does_not_inline_the_subcommand_list() {
        let cmd = crate::Cli::command();
        // Hyphenated names cannot collide with the hooks' own identifiers,
        // unlike `completion`, which appears verbatim in the zsh hook.
        let hyphenated: Vec<&str> = cmd
            .get_subcommands()
            .map(clap::Command::get_name)
            .filter(|n| n.contains('-'))
            .collect();
        assert!(
            !hyphenated.is_empty(),
            "expected hyphenated subcommands to test against"
        );

        for name in clap_complete::env::Shells::builtins().names() {
            let hook = hook_for(name);
            for sub in &hyphenated {
                assert!(
                    !hook.contains(sub),
                    "{name} hook inlines `{sub}` — the static script shipped:\n{hook}"
                );
            }
        }
    }

    /// The zsh hook is a compsys script, which is why the docs say to source it
    /// after `compinit`.
    #[test]
    fn zsh_hook_is_a_compdef_script() {
        assert!(
            hook_for("zsh").starts_with("#compdef cella"),
            "{}",
            hook_for("zsh")
        );
    }

    #[test]
    fn accepts_every_builtin_shell() {
        for name in clap_complete::env::Shells::builtins().names() {
            assert!(
                crate::Cli::try_parse_from(["cella", "completion", name]).is_ok(),
                "`cella completion {name}` must parse"
            );
        }
    }

    #[test]
    fn rejects_unknown_shell() {
        assert!(crate::Cli::try_parse_from(["cella", "completion", "nushell"]).is_err());
    }
}
