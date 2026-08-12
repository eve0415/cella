//! Rendering of [`CLI_SURFACE`](crate::CLI_SURFACE) into shell completion
//! scripts.
//!
//! A script is three parts concatenated: a generated data section, the shared
//! candidate engine, and a few lines of shell-specific registration. Only the
//! first is derived from the table — the engine's logic is fixed, so the two
//! shells cannot drift apart, and the table cannot drift from the parser.
//!
//! The data section is pure shell functions, no arrays and no unquoted word
//! splitting, so the identical text parses under bash, zsh and POSIX sh.

use crate::{CLI_SURFACE, CommandSpec, OptionSpec, find, root_commands, subcommands_of};

/// Kept at module level on purpose: a raw string inside a function would count
/// against clippy's `too_many_lines` budget, a `const` costs nothing.
const CORE_SH: &str = include_str!("../shell/core.sh");
const BASH_SH: &str = include_str!("../shell/bash.sh");
const ZSH_SH: &str = include_str!("../shell/zsh.sh");

const HEADER: &str = "# ---- generated from cella-completion::CLI_SURFACE — do not edit ----\n\n";

/// A self-contained bash completion script for the in-container `cella` CLI.
#[must_use]
pub fn bash_script() -> String {
    data_section() + CORE_SH + BASH_SH
}

/// A self-contained zsh completion script for the in-container `cella` CLI.
#[must_use]
pub fn zsh_script() -> String {
    data_section() + CORE_SH + ZSH_SH
}

/// The generated half: everything the engine needs to know about the surface.
fn data_section() -> String {
    let mut out = String::from(HEADER);
    out.push_str(&commands_fn());
    out.push_str(&subcommands_fn());
    out.push_str(&flags_fn());
    out.push_str(&predicate_fn("__cella_takes_value", &value_options()));
    out.push_str(&predicate_fn("__cella_takes_separator", &separator_paths()));
    out
}

/// Every spelling of one command's own word — canonical first, then aliases.
fn spellings(spec: &CommandSpec) -> Vec<&'static str> {
    let mut out = vec![spec.leaf()];
    out.extend(spec.aliases.iter().copied());
    out
}

/// Every spelling of a command's full path, e.g. `task list` and `task ls`.
fn path_spellings(spec: &CommandSpec) -> Vec<String> {
    let leaves = spellings(spec);
    let Some(parent) = spec.parent().and_then(find) else {
        return leaves.into_iter().map(str::to_owned).collect();
    };
    let mut keys = Vec::new();
    for prefix in path_spellings(parent) {
        for leaf in &leaves {
            keys.push(format!("{prefix} {leaf}"));
        }
    }
    keys
}

/// The path spellings as `case` patterns, quoted where they contain a space.
///
/// All of a command's spellings share one arm, so an alias cannot drift away
/// from the canonical name.
fn path_keys(spec: &CommandSpec) -> Vec<String> {
    path_spellings(spec)
        .into_iter()
        .map(|key| {
            if key.contains(' ') {
                format!("'{key}'")
            } else {
                key
            }
        })
        .collect()
}

/// Both spellings of an option, `--long` and `-s`.
fn option_spellings(opt: &OptionSpec) -> Vec<String> {
    let mut out = vec![opt.long.to_owned()];
    if let Some(short) = opt.short {
        out.push(format!("-{short}"));
    }
    out
}

/// A shell function wrapping a `case` over `$1`.
///
/// An empty arm list drops the `case` entirely — `case x in esac` is legal but
/// pointless, and this keeps the output honest when the table has nothing to
/// say.
fn case_fn(name: &str, arms: &[(String, String)], tail: &str) -> String {
    let mut cases = String::new();
    if !arms.is_empty() {
        cases.push_str("    case \"$1\" in\n");
        for (pattern, body) in arms {
            cases.push_str("        ");
            cases.push_str(pattern);
            cases.push_str(") ");
            cases.push_str(body);
            cases.push_str(" ;;\n");
        }
        cases.push_str("    esac\n");
    }
    format!("{name}() {{\n{cases}{tail}}}\n\n")
}

/// A `case` that answers yes (exit 0) for a fixed set of words.
fn predicate_fn(name: &str, patterns: &[String]) -> String {
    let arms = if patterns.is_empty() {
        Vec::new()
    } else {
        vec![(patterns.join("|"), "return 0".to_owned())]
    };
    case_fn(name, &arms, "    return 1\n")
}

/// The words accepted directly after `cella`, aliases included.
fn commands_fn() -> String {
    let words: Vec<&str> = root_commands().flat_map(spellings).collect();
    format!(
        "__cella_commands() {{ printf '%s\\n' {}; }}\n\n",
        words.join(" ")
    )
}

/// One arm per parent that actually has children in the table.
fn subcommands_fn() -> String {
    let mut arms = Vec::new();
    let mut seen: Vec<&str> = Vec::new();
    for spec in CLI_SURFACE {
        let Some(parent) = spec.parent() else {
            continue;
        };
        if seen.contains(&parent) {
            continue;
        }
        seen.push(parent);
        let words: Vec<&str> = subcommands_of(parent).flat_map(spellings).collect();
        let key = find(parent).map_or_else(|| parent.to_owned(), |p| path_keys(p).join("|"));
        arms.push((key, format!("printf '%s\\n' {}", words.join(" "))));
    }
    case_fn("__cella_subcommands", &arms, "")
}

/// One arm per command, keyed by its full path.
fn flags_fn() -> String {
    let mut arms = Vec::new();
    for spec in CLI_SURFACE {
        let words: Vec<String> = spec.options.iter().flat_map(option_spellings).collect();
        if words.is_empty() {
            continue;
        }
        arms.push((
            path_keys(spec).join("|"),
            format!("printf '%s\\n' {}", words.join(" ")),
        ));
    }
    case_fn("__cella_flags", &arms, "")
}

/// Every option anywhere in the table that consumes the following word.
fn value_options() -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for spec in CLI_SURFACE {
        for opt in spec.options.iter().filter(|opt| opt.value.is_some()) {
            for spelling in option_spellings(opt) {
                if !out.contains(&spelling) {
                    out.push(spelling);
                }
            }
        }
    }
    out
}

/// Every command path that takes a literal `--` passthrough.
fn separator_paths() -> Vec<String> {
    CLI_SURFACE
        .iter()
        .filter(|spec| spec.separator)
        .flat_map(path_keys)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{bash_script, data_section, zsh_script};
    use crate::{CLI_SURFACE, root_commands, subcommands_of};

    #[test]
    fn bash_script_is_valid_bash() {
        let out = std::process::Command::new("bash")
            .args(["-n", "-c", &bash_script()])
            .output()
            .expect("bash must be available");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Unconditional — a test that skips when zsh is missing gives no signal
    /// precisely when it matters. CI installs zsh.
    #[test]
    fn zsh_script_is_valid_zsh() {
        let out = std::process::Command::new("zsh")
            .args(["-n", "-c", &zsh_script()])
            .output()
            .expect("zsh must be available");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn generated_scripts_mention_every_flag() {
        for script in [bash_script(), zsh_script()] {
            for spec in CLI_SURFACE {
                for opt in spec.options {
                    assert!(
                        script.contains(opt.long),
                        "`{}` missing from script",
                        opt.long
                    );
                }
            }
        }
    }

    /// The one line the engine reads for `cella <TAB>`.
    fn commands_line() -> String {
        data_section()
            .lines()
            .find(|line| line.starts_with("__cella_commands()"))
            .expect("the data section must define __cella_commands")
            .to_owned()
    }

    #[test]
    fn every_root_command_and_alias_is_listed() {
        let line = commands_line();
        for spec in root_commands() {
            assert!(line.contains(spec.leaf()), "`{}` missing", spec.leaf());
            for alias in spec.aliases {
                assert!(line.contains(alias), "alias `{alias}` missing");
            }
        }
    }

    #[test]
    fn every_task_subcommand_is_listed() {
        let section = data_section();
        for spec in subcommands_of("task") {
            assert!(section.contains(spec.leaf()), "`{}` missing", spec.path);
            for alias in spec.aliases {
                assert!(section.contains(alias), "alias `{alias}` missing");
            }
        }
    }

    /// The body of one generated shell function, so an assertion cannot be
    /// satisfied by a match somewhere else in the data section.
    fn function_body(name: &str) -> String {
        let section = data_section();
        let (_, tail) = section
            .split_once(&format!("{name}()"))
            .unwrap_or_else(|| panic!("the data section must define {name}"));
        tail.split_once("\n}\n")
            .map_or_else(|| tail.to_owned(), |(body, _)| body.to_owned())
    }

    /// The engine skips an option's value by asking this predicate, so a value
    /// option missing here would make the next word complete as a flag.
    #[test]
    fn every_value_option_is_a_value_option_to_the_engine() {
        let body = function_body("__cella_takes_value");
        for spec in CLI_SURFACE {
            for opt in spec.options.iter().filter(|opt| opt.value.is_some()) {
                assert!(body.contains(opt.long), "`{}` takes a value", opt.long);
            }
        }
    }

    /// Asserts on the rendered `case` pattern, quoting included — that is the
    /// text the shell actually matches against.
    #[test]
    fn every_separator_command_is_known_to_the_engine() {
        let body = function_body("__cella_takes_separator");
        for spec in CLI_SURFACE.iter().filter(|spec| spec.separator) {
            for key in super::path_keys(spec) {
                assert!(body.contains(&key), "`{key}` takes a `--` separator");
            }
        }
    }

    /// Run the real generated script and ask the real engine.
    fn candidates(shell: &str, script: &str, call: &str) -> Vec<String> {
        let program = format!("{script}\n{call}\n");
        let out = std::process::Command::new(shell)
            .args(["-c", &program])
            .output()
            .unwrap_or_else(|err| panic!("{shell} must be available: {err}"));
        assert!(
            out.status.success(),
            "{shell} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            out.stderr.is_empty(),
            "{shell} wrote to stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// Both scripts, so every behavioural assertion below is made twice.
    fn engines() -> [(&'static str, String); 2] {
        [("bash", bash_script()), ("zsh", zsh_script())]
    }

    fn assert_candidates(call: &str, expected: &[&str]) {
        for (shell, script) in engines() {
            assert_eq!(
                candidates(shell, &script, call),
                expected,
                "{shell}: {call}"
            );
        }
    }

    #[test]
    fn the_first_word_completes_to_the_root_commands() {
        assert_candidates(
            r#"__cella_candidates "" cella"#,
            &[
                "branch", "list", "ls", "exec", "down", "up", "prune", "task", "switch", "doctor",
            ],
        );
    }

    #[test]
    fn a_parent_completes_to_its_children() {
        assert_candidates(
            r#"__cella_candidates "" cella task"#,
            &["run", "list", "ls", "logs", "wait", "stop"],
        );
    }

    #[test]
    fn a_dash_completes_to_the_commands_flags() {
        assert_candidates(
            r#"__cella_candidates "--" cella prune"#,
            &[
                "--all",
                "--dry-run",
                "--older-than",
                "--missing-worktree",
                "--label",
                "--help",
                "-h",
            ],
        );
    }

    #[test]
    fn a_sub_subcommand_resolves_its_own_flags() {
        assert_candidates(
            r#"__cella_candidates "-" cella task logs main"#,
            &["--follow", "-f", "--help", "-h"],
        );
    }

    /// An alias shares its canonical spelling's arm.
    #[test]
    fn an_alias_resolves_the_same_flags() {
        for call in [
            r#"__cella_candidates "--" cella task list"#,
            r#"__cella_candidates "--" cella task ls"#,
            r#"__cella_candidates "--" cella ls"#,
        ] {
            assert_candidates(call, &["--json", "--help", "-h"]);
        }
    }

    /// Nothing means "shell default" — branch names are not ours to guess.
    #[test]
    fn a_positional_slot_falls_back_to_the_shell() {
        assert_candidates(r#"__cella_candidates "" cella switch"#, &[]);
    }

    #[test]
    fn nothing_is_offered_past_the_separator() {
        assert_candidates(r#"__cella_candidates "" cella exec b --"#, &[]);
        assert_candidates(r#"__cella_candidates "--fl" cella exec b -- git"#, &[]);
    }

    #[test]
    fn nothing_is_offered_in_an_options_value_slot() {
        assert_candidates(r#"__cella_candidates "" cella branch x --base"#, &[]);
        assert_candidates(r#"__cella_candidates "-" cella branch x --base"#, &[]);
    }

    /// `--label KEY=VALUE` arrives split on `=` under bash's `COMP_WORDBREAKS`.
    ///
    /// The dash-leading case is the one that bites: without the guard,
    /// `--label KEY=-x` would complete `-x` against the command's flags.
    #[test]
    fn nothing_is_offered_after_a_bare_equals() {
        assert_candidates(r#"__cella_candidates "" cella branch x --label KEY ="#, &[]);
        assert_candidates(r#"__cella_candidates "-x" cella prune --label KEY ="#, &[]);
    }

    /// A flag sitting where a subcommand could go must not become the
    /// subcommand — `cella task --help <TAB>` still offers the children.
    #[test]
    fn a_flag_does_not_shadow_a_subcommand() {
        assert_candidates(
            r#"__cella_candidates "" cella task --help"#,
            &["run", "list", "ls", "logs", "wait", "stop"],
        );
    }

    #[test]
    fn a_flag_slot_before_any_command_offers_nothing() {
        assert_candidates(r#"__cella_candidates "-" cella"#, &[]);
    }
}
