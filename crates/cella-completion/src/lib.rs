//! The canonical description of the in-container `cella` command surface.
//!
//! Inside a dev container, `/cella/bin/cella` is the agent binary in CLI mode,
//! parsed by hand (`cella-agent/src/cli.rs`) rather than by clap — deliberately,
//! to keep the binary that ships into every container small. That leaves
//! nothing to derive help text or completions from, and the two promptly drift
//! from the parser.
//!
//! [`CLI_SURFACE`] is the one place the surface is written down. Three
//! consumers read it:
//!
//! 1. `cella-agent`'s help printers, at runtime.
//! 2. [`bash_script`] / [`zsh_script`], which `cella-docker` ships into the
//!    agent volume.
//! 3. A parity test in `cella-agent` that drives the real parser, so a table
//!    entry that the parser does not accept fails the build.
//!
//! This crate has no dependencies, by design: `cella-agent` links it at
//! runtime, so its cost is the table plus the rendering code and nothing else.

mod render;

pub use render::{bash_script, zsh_script};

/// One command or sub-subcommand of the in-container `cella` CLI.
pub struct CommandSpec {
    /// Space-separated path, e.g. `"branch"` or `"task run"`.
    pub path: &'static str,
    /// Alternate spellings the parser accepts, e.g. `["ls"]` for `"list"`.
    pub aliases: &'static [&'static str],
    /// One-line description, rendered into help output.
    pub about: &'static str,
    /// Positional arguments, in order.
    pub operands: &'static [OperandSpec],
    /// Every option the parser accepts for this command.
    pub options: &'static [OptionSpec],
    /// True when a literal `--` introduces a passthrough command.
    pub separator: bool,
}

impl CommandSpec {
    /// The last segment of [`Self::path`] — the word typed at this level.
    #[must_use]
    pub fn leaf(&self) -> &'static str {
        self.path.rsplit(' ').next().unwrap_or(self.path)
    }

    /// The path of the parent command, or `None` for a root command.
    #[must_use]
    pub fn parent(&self) -> Option<&'static str> {
        self.path.rsplit_once(' ').map(|(parent, _)| parent)
    }

    /// Whether this command is spelled `name`, under any of its aliases.
    #[must_use]
    pub fn matches(&self, name: &str) -> bool {
        self.leaf() == name || self.aliases.contains(&name)
    }
}

/// One option (`--flag`, or `--opt <value>`) accepted by a command.
pub struct OptionSpec {
    /// Long spelling, including the leading `--`.
    pub long: &'static str,
    /// Short spelling, without the leading `-`.
    pub short: Option<char>,
    /// `Some` when the option consumes the following argument.
    pub value: Option<ValueSpec>,
    /// How many times the option may be given.
    pub occurrence: Occurrence,
    /// One-line description, rendered into help output.
    pub help: &'static str,
}

/// How many times an option may appear.
pub enum Occurrence {
    /// A boolean switch, taking no value.
    Flag,
    /// Accepted once; a later occurrence overwrites the earlier.
    Once,
    /// Accumulates across occurrences.
    Repeatable,
}

/// The value an option consumes.
pub struct ValueSpec {
    /// Placeholder shown in help, e.g. `"ref"`, `"dur"`, `"KEY=VALUE"`.
    pub placeholder: &'static str,
}

/// A positional argument.
pub enum OperandSpec {
    /// Must be given.
    Required {
        /// Placeholder shown in help.
        name: &'static str,
    },
    /// May be omitted.
    Optional {
        /// Placeholder shown in help.
        name: &'static str,
    },
    /// Everything after `--`.
    Trailing {
        /// Placeholder shown in help.
        name: &'static str,
    },
}

impl OperandSpec {
    /// Render the operand as it appears in a synopsis.
    #[must_use]
    pub fn synopsis(&self) -> String {
        match self {
            Self::Required { name } => format!("<{name}>"),
            Self::Optional { name } => format!("[{name}]"),
            Self::Trailing { name } => format!("-- <{name}...>"),
        }
    }
}

/// `--help` / `-h`, which the parser intercepts for every command
/// (`cella-agent/src/cli.rs`, before any per-command parsing runs).
const HELP: OptionSpec = OptionSpec {
    long: "--help",
    short: Some('h'),
    value: None,
    occurrence: Occurrence::Flag,
    help: "Show this help message",
};

const JSON_ARRAY: OptionSpec = OptionSpec {
    long: "--json",
    short: None,
    value: None,
    occurrence: Occurrence::Flag,
    help: "Output as JSON array",
};

const BASE: OptionSpec = OptionSpec {
    long: "--base",
    short: None,
    value: Some(ValueSpec { placeholder: "ref" }),
    occurrence: Occurrence::Once,
    help: "Base branch or commit (default: current HEAD)",
};

/// Every command the in-container CLI accepts.
///
/// Derived from the parser in `cella-agent/src/cli.rs`, not from the help text
/// it used to print — the help text had already drifted. Prose is lifted from
/// the previous hand-written help so the rendered output stays recognisable.
pub const CLI_SURFACE: &[CommandSpec] = &[
    CommandSpec {
        path: "branch",
        aliases: &[],
        about: "Create a worktree-backed branch with its own container.",
        operands: &[OperandSpec::Required { name: "name" }],
        options: &[
            BASE,
            OptionSpec {
                long: "--label",
                short: None,
                value: Some(ValueSpec {
                    placeholder: "KEY=VALUE",
                }),
                occurrence: Occurrence::Repeatable,
                help: "Add a label to the container (repeatable)",
            },
            HELP,
        ],
        separator: false,
    },
    CommandSpec {
        path: "list",
        aliases: &["ls"],
        about: "List worktree branches and their container status.",
        operands: &[],
        options: &[JSON_ARRAY, HELP],
        separator: false,
    },
    CommandSpec {
        path: "exec",
        aliases: &[],
        about: "Run a command in another branch's container.",
        operands: &[
            OperandSpec::Required { name: "branch" },
            OperandSpec::Trailing { name: "command" },
        ],
        options: &[
            OptionSpec {
                long: "--json",
                short: None,
                value: None,
                occurrence: Occurrence::Flag,
                help: "Capture stdout/stderr and output as JSON envelope",
            },
            HELP,
        ],
        separator: true,
    },
    CommandSpec {
        path: "down",
        aliases: &[],
        about: "Stop a worktree branch's container.",
        operands: &[OperandSpec::Required { name: "branch" }],
        options: &[
            OptionSpec {
                long: "--rm",
                short: None,
                value: None,
                occurrence: Occurrence::Flag,
                help: "Remove the container and worktree after stopping",
            },
            OptionSpec {
                long: "--volumes",
                short: None,
                value: None,
                occurrence: Occurrence::Flag,
                help: "Also remove volumes (requires --rm)",
            },
            OptionSpec {
                long: "--force",
                short: None,
                value: None,
                occurrence: Occurrence::Flag,
                help: "Force stop even when shutdownAction is \"none\"",
            },
            HELP,
        ],
        separator: false,
    },
    CommandSpec {
        path: "up",
        aliases: &[],
        about: "Start or restart a worktree branch's container.",
        operands: &[OperandSpec::Required { name: "branch" }],
        options: &[
            OptionSpec {
                long: "--rebuild",
                short: None,
                value: None,
                occurrence: Occurrence::Flag,
                help: "Rebuild the container from scratch",
            },
            HELP,
        ],
        separator: false,
    },
    CommandSpec {
        path: "prune",
        aliases: &[],
        about: "Remove worktrees and their containers.",
        operands: &[],
        options: &[
            OptionSpec {
                long: "--all",
                short: None,
                value: None,
                occurrence: Occurrence::Flag,
                help: "Include unmerged worktrees",
            },
            OptionSpec {
                long: "--dry-run",
                short: None,
                value: None,
                occurrence: Occurrence::Flag,
                help: "Show what would be pruned without doing it",
            },
            OptionSpec {
                long: "--older-than",
                short: None,
                value: Some(ValueSpec { placeholder: "dur" }),
                occurrence: Occurrence::Once,
                help: "Only prune older than duration (e.g., 7d, 24h)",
            },
            OptionSpec {
                long: "--missing-worktree",
                short: None,
                value: None,
                occurrence: Occurrence::Flag,
                help: "Only prune branches whose worktree is gone",
            },
            OptionSpec {
                long: "--label",
                short: None,
                value: Some(ValueSpec {
                    placeholder: "KEY=VALUE",
                }),
                occurrence: Occurrence::Repeatable,
                help: "Only prune matching labels (repeatable)",
            },
            HELP,
        ],
        separator: false,
    },
    CommandSpec {
        path: "task",
        aliases: &[],
        about: "Run and manage background tasks.",
        operands: &[],
        options: &[HELP],
        separator: false,
    },
    CommandSpec {
        path: "task run",
        aliases: &[],
        about: "Run a background task",
        operands: &[
            OperandSpec::Required { name: "branch" },
            OperandSpec::Trailing { name: "command" },
        ],
        options: &[
            BASE,
            OptionSpec {
                long: "--timeout",
                short: None,
                value: Some(ValueSpec {
                    placeholder: "secs",
                }),
                occurrence: Occurrence::Once,
                help: "Timeout in seconds (e.g., 300)",
            },
            HELP,
        ],
        separator: true,
    },
    CommandSpec {
        path: "task list",
        aliases: &["ls"],
        about: "List active tasks",
        operands: &[],
        options: &[JSON_ARRAY, HELP],
        separator: false,
    },
    CommandSpec {
        path: "task logs",
        aliases: &[],
        about: "Show task output",
        operands: &[OperandSpec::Required { name: "branch" }],
        options: &[
            OptionSpec {
                long: "--follow",
                short: Some('f'),
                value: None,
                occurrence: Occurrence::Flag,
                help: "Follow the output",
            },
            HELP,
        ],
        separator: false,
    },
    CommandSpec {
        path: "task wait",
        aliases: &[],
        about: "Wait for task completion",
        operands: &[OperandSpec::Required { name: "branch" }],
        options: &[HELP],
        separator: false,
    },
    CommandSpec {
        path: "task stop",
        aliases: &[],
        about: "Stop a running task",
        operands: &[OperandSpec::Required { name: "branch" }],
        options: &[HELP],
        separator: false,
    },
    CommandSpec {
        path: "switch",
        aliases: &[],
        about: "Open an interactive shell in another branch's container.",
        operands: &[OperandSpec::Required { name: "branch" }],
        options: &[HELP],
        separator: false,
    },
    CommandSpec {
        path: "doctor",
        aliases: &[],
        about: "Check daemon connectivity and version status.",
        operands: &[],
        options: &[
            OptionSpec {
                long: "--json",
                short: None,
                value: None,
                occurrence: Occurrence::Flag,
                help: "Output structured health data as JSON",
            },
            HELP,
        ],
        separator: false,
    },
];

/// The commands typed directly after `cella`.
pub fn root_commands() -> impl Iterator<Item = &'static CommandSpec> {
    CLI_SURFACE.iter().filter(|c| c.parent().is_none())
}

/// The sub-subcommands of `parent`, e.g. `run`/`list`/… for `"task"`.
pub fn subcommands_of(parent: &str) -> impl Iterator<Item = &'static CommandSpec> + '_ {
    CLI_SURFACE
        .iter()
        .filter(move |c| c.parent() == Some(parent))
}

/// Look up one command by its exact [`CommandSpec::path`].
#[must_use]
pub fn find(path: &str) -> Option<&'static CommandSpec> {
    CLI_SURFACE.iter().find(|c| c.path == path)
}

#[cfg(test)]
mod tests {
    use super::{CLI_SURFACE, Occurrence, find, root_commands, subcommands_of};

    #[test]
    fn every_path_is_unique() {
        let mut seen = Vec::new();
        for spec in CLI_SURFACE {
            assert!(!seen.contains(&spec.path), "duplicate path `{}`", spec.path);
            seen.push(spec.path);
        }
    }

    /// Aliases collide only within one parent scope — `list` and `task list`
    /// both answer to `ls`, and must be allowed to.
    #[test]
    fn spellings_are_unique_within_a_scope() {
        for spec in CLI_SURFACE {
            let mut seen = vec![spec.leaf()];
            for alias in spec.aliases {
                assert!(
                    !seen.contains(alias),
                    "`{}` repeats the spelling `{alias}`",
                    spec.path
                );
                seen.push(alias);
            }
            for sibling in CLI_SURFACE {
                if sibling.path == spec.path || sibling.parent() != spec.parent() {
                    continue;
                }
                for spelling in &seen {
                    assert!(
                        !sibling.matches(spelling),
                        "`{}` and `{}` both answer to `{spelling}`",
                        spec.path,
                        sibling.path
                    );
                }
            }
        }
    }

    #[test]
    fn every_subcommand_has_a_parent_in_the_table() {
        for spec in CLI_SURFACE {
            if let Some(parent) = spec.parent() {
                assert!(
                    find(parent).is_some(),
                    "`{}` has no parent entry `{parent}`",
                    spec.path
                );
            }
        }
    }

    #[test]
    fn every_command_describes_itself() {
        for spec in CLI_SURFACE {
            assert!(!spec.about.is_empty(), "`{}` has no about", spec.path);
            for opt in spec.options {
                assert!(
                    !opt.help.is_empty(),
                    "`{} {}` has no help",
                    spec.path,
                    opt.long
                );
            }
        }
    }

    #[test]
    fn every_option_is_well_formed() {
        for spec in CLI_SURFACE {
            let mut longs = Vec::new();
            let mut shorts = Vec::new();
            for opt in spec.options {
                assert!(
                    opt.long.starts_with("--") && opt.long.len() > 2,
                    "`{}` option `{}` is not a long flag",
                    spec.path,
                    opt.long
                );
                assert!(
                    !longs.contains(&opt.long),
                    "`{}` repeats `{}`",
                    spec.path,
                    opt.long
                );
                longs.push(opt.long);
                if let Some(short) = opt.short {
                    assert!(
                        !shorts.contains(&short),
                        "`{}` repeats `-{short}`",
                        spec.path
                    );
                    shorts.push(short);
                }
                // A repeatable switch would have nothing to accumulate.
                if matches!(opt.occurrence, Occurrence::Repeatable) {
                    assert!(
                        opt.value.is_some(),
                        "`{} {}` is repeatable but takes no value",
                        spec.path,
                        opt.long
                    );
                }
                if let Some(value) = &opt.value {
                    assert!(
                        !value.placeholder.is_empty(),
                        "`{} {}` has an empty placeholder",
                        spec.path,
                        opt.long
                    );
                    assert!(
                        !matches!(opt.occurrence, Occurrence::Flag),
                        "`{} {}` takes a value but is a Flag",
                        spec.path,
                        opt.long
                    );
                }
            }
        }
    }

    /// A `--` passthrough is meaningless without a trailing operand to hold it.
    #[test]
    fn separator_commands_have_a_trailing_operand() {
        for spec in CLI_SURFACE {
            let trailing = spec
                .operands
                .iter()
                .any(|o| matches!(o, super::OperandSpec::Trailing { .. }));
            assert_eq!(
                spec.separator, trailing,
                "`{}` separator/trailing-operand mismatch",
                spec.path
            );
        }
    }

    #[test]
    fn every_command_accepts_help() {
        for spec in CLI_SURFACE {
            assert!(
                spec.options.iter().any(|o| o.long == "--help"),
                "`{}` must accept --help; the parser intercepts it everywhere",
                spec.path
            );
        }
    }

    #[test]
    fn the_table_partitions_into_roots_and_children() {
        assert_eq!(
            root_commands().count() + subcommands_of("task").count(),
            CLI_SURFACE.len(),
            "every entry must be a root or a `task` subcommand"
        );
        assert_eq!(root_commands().count(), 9);
        assert_eq!(subcommands_of("task").count(), 5);
    }
}
