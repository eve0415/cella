pub mod backend;
mod commands;
pub mod picker;
pub mod progress;
pub mod style;
mod table;
mod title;

use std::io::IsTerminal;

use clap::Parser;
use tracing_subscriber::EnvFilter;

use commands::{LogFormat, LogLevel};
use progress::{IndicatifMakeWriter, Progress};

/// Map `--log-level` (and `RUST_LOG` precedence) to a tracing `EnvFilter`
/// directive string.
///
/// Precedence: `RUST_LOG` (highest) > `--log-level` > default `info`. When
/// `RUST_LOG` is set we return `None` so the caller falls back to
/// `EnvFilter::from_default_env()` and the developer's env var wins untouched.
///
/// `--log-level debug`/`trace` is scoped to the `cella` crates with a lower
/// `info` global fallback (`cella=debug,info`) so raising cella's verbosity
/// doesn't drown stderr in dependency-crate logs (bollard, hyper, gix). This
/// crate-scoping is a cella choice; the official CLI has a flat numeric level.
fn resolve_log_directive(rust_log_set: bool, level: Option<LogLevel>) -> Option<String> {
    if rust_log_set {
        return None;
    }
    Some(match level.unwrap_or(LogLevel::Info) {
        // Default/info: a flat `info` directive matches the official default
        // (events at info and above are shown).
        LogLevel::Info => "info".to_string(),
        // debug/trace: raise cella's crates only, keep the rest at info.
        LogLevel::Debug => "cella=debug,info".to_string(),
        LogLevel::Trace => "cella=trace,info".to_string(),
    })
}

/// Decide whether indicatif spinners may render on stderr.
///
/// Spinners require all of: text output mode, no `RUST_LOG` (its logs would
/// interleave with spinner frames), a TTY, and a non-`Json` log format —
/// `--log-format json` writes machine-readable JSON log lines to stderr and
/// indicatif's ANSI escapes would corrupt them.
const fn spinners_enabled(
    is_text_output: bool,
    rust_log_set: bool,
    is_tty: bool,
    log_format: LogFormat,
) -> bool {
    is_text_output && !rust_log_set && is_tty && matches!(log_format, LogFormat::Text)
}

/// Build the `EnvFilter` for the global subscriber, honoring `RUST_LOG` first
/// and falling back to the `--log-level`-derived directive.
fn build_env_filter(rust_log_set: bool, level: Option<LogLevel>) -> EnvFilter {
    resolve_log_directive(rust_log_set, level)
        .map_or_else(EnvFilter::from_default_env, EnvFilter::new)
}

/// cella — Dev containers reinvented for the AI age
#[derive(Parser)]
#[command(name = "cella", version, about, disable_version_flag = true)]
struct Cli {
    /// Print version.
    #[arg(short = 'v', long = "version", action = clap::ArgAction::Version)]
    _version: (),

    #[command(subcommand)]
    command: commands::Command,
}

#[tokio::main]
async fn main() {
    title::install_signal_handlers();

    // Install miette's graphical error handler for pretty diagnostics
    miette::set_hook(Box::new(|_| {
        Box::new(miette::GraphicalReportHandler::new_themed(
            miette::GraphicalTheme::unicode(),
        ))
    }))
    .ok();

    // Parse CLI first to determine output mode before creating progress.
    let cli = Cli::parse();

    let verbosity = cli.command.verbosity();

    // `--log-level` / `--log-format` are parsed into the `up` subcommand args,
    // but the global subscriber is installed here, once, before dispatch — so
    // read them off the parsed command rather than inside `execute()`.
    let rust_log_set = std::env::var_os("RUST_LOG").is_some();
    let is_tty = std::io::stderr().is_terminal();
    let log_format = cli.command.log_format();
    let spinners_enabled = spinners_enabled(
        cli.command.is_text_output(),
        rust_log_set,
        is_tty,
        log_format,
    );

    let progress = Progress::new(spinners_enabled, verbosity);

    // The daemon subprocess initializes its own file-based tracing.
    // Skip the normal indicatif-based tracing for daemon start.
    if !cli.command.is_daemon_start() {
        let env_filter = build_env_filter(rust_log_set, cli.command.log_level());
        if matches!(log_format, LogFormat::Json) {
            // `--log-format json`: emit JSON log lines on stderr. Spinners are
            // already forced off above. Note: cella's tracing `.json()` shape
            // (string level, target/span fields) is its own schema, NOT the
            // official LogEvent shape (numeric level, epoch timestamps) — a
            // deliberate divergence, see the logging-terminal spec.
            tracing_subscriber::fmt()
                .json()
                .with_env_filter(env_filter)
                .with_writer(std::io::stderr)
                .init();
        } else if spinners_enabled {
            // Route tracing through indicatif so log lines don't corrupt spinners.
            tracing_subscriber::fmt()
                .with_env_filter(env_filter)
                .with_writer(IndicatifMakeWriter::new(progress.multi().clone()))
                .init();
        } else {
            // No spinners (RUST_LOG set, non-TTY): write directly to stderr.
            // Spec requires stdout = JSON only, stderr = logs only.
            tracing_subscriber::fmt()
                .with_env_filter(env_filter)
                .with_writer(std::io::stderr)
                .init();
        }
    }

    // The `up` argument surface is large (full devcontainer-CLI flag parity),
    // so box the dispatch future to keep it off the stack.
    if let Err(report) = Box::pin(cli.command.execute(progress)).await {
        eprintln!("{report:?}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::Cli;

    // Helper to parse CLI args. Returns the parsed Cli or the error.
    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(args)
    }

    // ── up command ──────────────────────────────────────────────────

    #[test]
    fn parse_up_minimal() {
        let cli = parse(&["cella", "up"]).unwrap();
        assert!(matches!(cli.command, super::commands::Command::Up(_)));
    }

    #[test]
    fn parse_up_with_verbose() {
        let cli = parse(&["cella", "up", "--verbose"]).unwrap();
        if let super::commands::Command::Up(args) = &cli.command {
            assert!(args.verbose.verbose);
        } else {
            panic!("expected Up command");
        }
    }

    #[test]
    fn parse_up_with_rebuild() {
        let cli = parse(&["cella", "up", "--rebuild"]).unwrap();
        if let super::commands::Command::Up(args) = &cli.command {
            assert!(args.build.rebuild);
        } else {
            panic!("expected Up command");
        }
    }

    #[test]
    fn parse_up_with_workspace_folder() {
        let cli = parse(&["cella", "up", "--workspace-folder", "/tmp/ws"]).unwrap();
        if let super::commands::Command::Up(args) = &cli.command {
            assert_eq!(
                args.workspace_folder.as_deref(),
                Some(std::path::Path::new("/tmp/ws"))
            );
        } else {
            panic!("expected Up command");
        }
    }

    #[test]
    fn parse_up_json_output() {
        let cli = parse(&["cella", "up", "--output", "json"]).unwrap();
        if let super::commands::Command::Up(args) = &cli.command {
            assert!(!args.is_text_output());
        } else {
            panic!("expected Up command");
        }
    }

    #[test]
    fn parse_up_text_output() {
        let cli = parse(&["cella", "up", "--output", "text"]).unwrap();
        if let super::commands::Command::Up(args) = &cli.command {
            assert!(args.is_text_output());
        } else {
            panic!("expected Up command");
        }
    }

    #[test]
    fn parse_up_all_flags() {
        let cli = parse(&[
            "cella",
            "up",
            "--verbose",
            "--rebuild",
            "--build-no-cache",
            "--remove-existing-container",
            "--skip-checksum",
            "--no-network-rules",
        ])
        .unwrap();
        if let super::commands::Command::Up(args) = &cli.command {
            assert!(args.verbose.verbose);
            assert!(args.build.rebuild);
            assert!(args.build.build_no_cache);
            assert!(args.build.remove_existing_container);
            assert!(args.skip_checksum);
            assert!(args.no_network_rules);
        } else {
            panic!("expected Up command");
        }
    }

    // ── down command ────────────────────────────────────────────────

    #[test]
    fn parse_down_minimal() {
        let cli = parse(&["cella", "down"]).unwrap();
        assert!(matches!(cli.command, super::commands::Command::Down(_)));
    }

    #[test]
    fn parse_down_with_rm() {
        // --rm is accepted
        let cli = parse(&["cella", "down", "--rm"]).unwrap();
        assert!(matches!(cli.command, super::commands::Command::Down(_)));
    }

    #[test]
    fn parse_down_volumes_requires_rm() {
        let result = parse(&["cella", "down", "--volumes"]);
        assert!(result.is_err());
    }

    #[test]
    fn parse_down_rm_with_volumes() {
        let cli = parse(&["cella", "down", "--rm", "--volumes"]).unwrap();
        assert!(matches!(cli.command, super::commands::Command::Down(_)));
    }

    #[test]
    fn parse_down_with_force() {
        let cli = parse(&["cella", "down", "--force"]).unwrap();
        assert!(matches!(cli.command, super::commands::Command::Down(_)));
    }

    #[test]
    fn parse_down_json_output() {
        let cli = parse(&["cella", "down", "--output", "json"]).unwrap();
        assert!(!cli.command.is_text_output());
    }

    // ── exec command ────────────────────────────────────────────────

    #[test]
    fn parse_exec_with_command() {
        let cli = parse(&["cella", "exec", "ls", "-la"]).unwrap();
        assert!(matches!(cli.command, super::commands::Command::Exec(_)));
    }

    #[test]
    fn parse_exec_requires_command() {
        let result = parse(&["cella", "exec"]);
        assert!(result.is_err());
    }

    #[test]
    fn parse_exec_with_user() {
        let cli = parse(&["cella", "exec", "--user", "root", "whoami"]).unwrap();
        assert!(matches!(cli.command, super::commands::Command::Exec(_)));
    }

    #[test]
    fn parse_exec_with_detach() {
        let cli = parse(&["cella", "exec", "-d", "sleep", "100"]).unwrap();
        assert!(matches!(cli.command, super::commands::Command::Exec(_)));
    }

    #[test]
    fn parse_exec_with_remote_env() {
        let cli = parse(&[
            "cella",
            "exec",
            "--remote-env",
            "FOO=bar",
            "--remote-env",
            "BAZ=qux",
            "env",
        ])
        .unwrap();
        assert!(matches!(cli.command, super::commands::Command::Exec(_)));
    }

    // ── list command ────────────────────────────────────────────────

    #[test]
    fn parse_list_minimal() {
        let cli = parse(&["cella", "list"]).unwrap();
        assert!(matches!(cli.command, super::commands::Command::List(_)));
    }

    #[test]
    fn parse_list_running() {
        let cli = parse(&["cella", "list", "--running"]).unwrap();
        assert!(matches!(cli.command, super::commands::Command::List(_)));
    }

    #[test]
    fn parse_list_output_json() {
        let cli = parse(&["cella", "list", "--output", "json"]).unwrap();
        assert!(matches!(cli.command, super::commands::Command::List(_)));
    }

    // ── doctor command ──────────────────────────────────────────────

    #[test]
    fn parse_doctor_minimal() {
        let cli = parse(&["cella", "doctor"]).unwrap();
        assert!(matches!(cli.command, super::commands::Command::Doctor(_)));
    }

    #[test]
    fn parse_doctor_all() {
        let cli = parse(&["cella", "doctor", "--all"]).unwrap();
        assert!(matches!(cli.command, super::commands::Command::Doctor(_)));
    }

    #[test]
    fn parse_doctor_output_json() {
        let cli = parse(&["cella", "doctor", "--output", "json"]).unwrap();
        assert!(matches!(cli.command, super::commands::Command::Doctor(_)));
    }

    #[test]
    fn parse_doctor_no_redact() {
        let cli = parse(&["cella", "doctor", "--no-redact"]).unwrap();
        assert!(matches!(cli.command, super::commands::Command::Doctor(_)));
    }

    #[test]
    fn parse_doctor_all_flags() {
        let cli = parse(&[
            "cella",
            "doctor",
            "--all",
            "--output",
            "json",
            "--no-redact",
        ])
        .unwrap();
        assert!(matches!(cli.command, super::commands::Command::Doctor(_)));
    }

    // ── branch command ──────────────────────────────────────────────

    #[test]
    fn parse_branch_with_name() {
        let cli = parse(&["cella", "branch", "feature/new-thing"]).unwrap();
        assert!(matches!(cli.command, super::commands::Command::Branch(_)));
    }

    #[test]
    fn parse_branch_requires_name() {
        let result = parse(&["cella", "branch"]);
        assert!(result.is_err());
    }

    #[test]
    fn parse_branch_with_base() {
        let cli = parse(&["cella", "branch", "feat/x", "--base", "main"]).unwrap();
        assert!(matches!(cli.command, super::commands::Command::Branch(_)));
    }

    #[test]
    fn parse_branch_with_exec() {
        let cli = parse(&["cella", "branch", "feat/x", "--exec", "npm install"]).unwrap();
        assert!(matches!(cli.command, super::commands::Command::Branch(_)));
    }

    #[test]
    fn parse_branch_with_output_json() {
        let cli = parse(&["cella", "branch", "feat/x", "--output", "json"]).unwrap();
        assert!(matches!(cli.command, super::commands::Command::Branch(_)));
    }

    // ── prune command ───────────────────────────────────────────────

    #[test]
    fn parse_prune_minimal() {
        let cli = parse(&["cella", "prune"]).unwrap();
        assert!(matches!(cli.command, super::commands::Command::Prune(_)));
    }

    #[test]
    fn parse_prune_force() {
        let cli = parse(&["cella", "prune", "--force"]).unwrap();
        assert!(matches!(cli.command, super::commands::Command::Prune(_)));
    }

    #[test]
    fn parse_prune_dry_run() {
        let cli = parse(&["cella", "prune", "--dry-run"]).unwrap();
        assert!(matches!(cli.command, super::commands::Command::Prune(_)));
    }

    #[test]
    fn parse_prune_all() {
        let cli = parse(&["cella", "prune", "--all"]).unwrap();
        assert!(matches!(cli.command, super::commands::Command::Prune(_)));
    }

    #[test]
    fn parse_prune_all_flags() {
        let cli = parse(&["cella", "prune", "--force", "--dry-run", "--all"]).unwrap();
        assert!(matches!(cli.command, super::commands::Command::Prune(_)));
    }

    // ── build command ───────────────────────────────────────────────

    #[test]
    fn parse_build_minimal() {
        let cli = parse(&["cella", "build"]).unwrap();
        assert!(matches!(cli.command, super::commands::Command::Build(_)));
    }

    #[test]
    fn parse_build_no_cache() {
        let cli = parse(&["cella", "build", "--no-cache"]).unwrap();
        assert!(matches!(cli.command, super::commands::Command::Build(_)));
    }

    #[test]
    fn parse_build_with_workspace_and_config() {
        let cli = parse(&[
            "cella",
            "build",
            "--workspace-folder",
            "/tmp",
            "--config",
            "/tmp/.devcontainer/devcontainer.json",
        ])
        .unwrap();
        assert!(matches!(cli.command, super::commands::Command::Build(_)));
    }

    // ── pull policy enums ────────────────────────────────────────────

    #[test]
    fn parse_up_pull_always() {
        let cli = parse(&["cella", "up", "--pull", "always"]).unwrap();
        assert!(matches!(cli.command, super::commands::Command::Up(_)));
    }

    #[test]
    fn parse_up_pull_invalid() {
        let result = parse(&["cella", "up", "--pull", "invalid"]);
        assert!(result.is_err());
    }

    #[test]
    fn parse_up_pull_policy_build() {
        let cli = parse(&["cella", "up", "--pull-policy", "build"]).unwrap();
        assert!(matches!(cli.command, super::commands::Command::Up(_)));
    }

    #[test]
    fn parse_up_strict_host_requirements() {
        let cli = parse(&["cella", "up", "--strict", "host-requirements"]).unwrap();
        assert!(matches!(cli.command, super::commands::Command::Up(_)));
    }

    #[test]
    fn parse_up_strict_invalid() {
        let result = parse(&["cella", "up", "--strict", "invalid"]);
        assert!(result.is_err());
    }

    // ── code command ────────────────────────────────────────────────

    #[test]
    fn parse_code_minimal() {
        let cli = parse(&["cella", "code"]).unwrap();
        assert!(matches!(cli.command, super::commands::Command::Code(_)));
    }

    #[test]
    fn parse_code_editor_insiders() {
        let cli = parse(&["cella", "code", "--editor", "insiders"]).unwrap();
        assert!(matches!(cli.command, super::commands::Command::Code(_)));
    }

    #[test]
    fn parse_code_editor_cursor() {
        let cli = parse(&["cella", "code", "--editor", "cursor"]).unwrap();
        assert!(matches!(cli.command, super::commands::Command::Code(_)));
    }

    #[test]
    fn parse_code_editor_invalid() {
        let result = parse(&["cella", "code", "--editor", "vim"]);
        assert!(result.is_err());
    }

    #[test]
    fn parse_code_binary() {
        let cli = parse(&["cella", "code", "--binary", "my-editor"]).unwrap();
        if let super::commands::Command::Code(args) = cli.command {
            assert_eq!(args.binary.as_deref(), Some("my-editor"));
        } else {
            panic!("expected Code command");
        }
    }

    #[test]
    fn parse_code_binary_conflicts_with_editor() {
        let result = parse(&["cella", "code", "--binary", "x", "--editor", "cursor"]);
        assert!(result.is_err());
    }

    // ── install command ──────────────────────────────────────────────

    #[test]
    fn parse_install_minimal() {
        let cli = parse(&["cella", "install"]).unwrap();
        assert!(matches!(cli.command, super::commands::Command::Install(_)));
    }

    #[test]
    fn parse_install_with_tools() {
        let cli = parse(&["cella", "install", "claude-code", "nvim"]).unwrap();
        if let super::commands::Command::Install(args) = cli.command {
            assert_eq!(args.tools, vec!["claude-code", "nvim"]);
        } else {
            panic!("expected Install command");
        }
    }

    #[test]
    fn parse_install_all() {
        let cli = parse(&["cella", "install", "--all"]).unwrap();
        if let super::commands::Command::Install(args) = cli.command {
            assert!(args.all);
        } else {
            panic!("expected Install command");
        }
    }

    // ── network command ─────────────────────────────────────────────

    #[test]
    fn parse_network_status() {
        let cli = parse(&["cella", "network", "status"]).unwrap();
        assert!(matches!(cli.command, super::commands::Command::Network(_)));
    }

    #[test]
    fn parse_network_test() {
        let cli = parse(&["cella", "network", "test", "https://example.com"]).unwrap();
        assert!(matches!(cli.command, super::commands::Command::Network(_)));
    }

    #[test]
    fn parse_network_log() {
        let cli = parse(&["cella", "network", "log"]).unwrap();
        assert!(matches!(cli.command, super::commands::Command::Network(_)));
    }

    #[test]
    fn parse_network_requires_subcommand() {
        let result = parse(&["cella", "network"]);
        assert!(result.is_err());
    }

    // ── per-command backend flag ──────────────────────────────────────

    #[test]
    fn parse_up_with_backend_docker() {
        let cli = parse(&["cella", "up", "--backend", "docker"]).unwrap();
        if let super::commands::Command::Up(args) = &cli.command {
            assert!(args.backend.backend.is_some());
        } else {
            panic!("expected Up command");
        }
    }

    #[test]
    fn parse_list_with_backend_and_docker_host() {
        let cli = parse(&[
            "cella",
            "list",
            "--backend",
            "docker",
            "--docker-host",
            "tcp://host:2375",
        ])
        .unwrap();
        assert!(matches!(cli.command, super::commands::Command::List(_)));
    }

    #[test]
    fn parse_backend_invalid_value() {
        let result = parse(&["cella", "list", "--backend", "invalid"]);
        assert!(result.is_err());
    }

    #[test]
    fn parse_no_backend_is_none() {
        // Without --backend, parsing should still succeed
        let cli = parse(&["cella", "list"]).unwrap();
        assert!(matches!(cli.command, super::commands::Command::List(_)));
    }

    #[test]
    fn parse_config_rejects_backend() {
        // Commands that don't use BackendArgs should reject --backend
        let result = parse(&["cella", "config", "show", "--backend", "docker"]);
        assert!(result.is_err());
    }

    #[test]
    fn parse_global_backend_no_longer_accepted() {
        // --backend before the subcommand should fail (it's no longer global)
        let result = parse(&["cella", "--backend", "docker", "list"]);
        assert!(result.is_err());
    }

    // ── Command::is_text_output / verbosity / is_daemon_start ──────

    #[test]
    fn up_default_keeps_spinners_on() {
        // Default `--output auto` must keep spinners enabled (is_text_output
        // is the spinner gate, independent of the resolved stdout format).
        let cli = parse(&["cella", "up"]).unwrap();
        assert!(cli.command.is_text_output());
    }

    #[test]
    fn up_json_is_not_text_output() {
        let cli = parse(&["cella", "up", "--output", "json"]).unwrap();
        assert!(!cli.command.is_text_output());
    }

    #[test]
    fn list_is_always_text_output() {
        let cli = parse(&["cella", "list"]).unwrap();
        assert!(cli.command.is_text_output());
    }

    #[test]
    fn read_configuration_is_not_text_output() {
        let cli = parse(&["cella", "read-configuration"]).unwrap();
        assert!(!cli.command.is_text_output());
    }

    #[test]
    fn up_default_verbosity_is_normal() {
        use crate::progress::Verbosity;
        let cli = parse(&["cella", "up"]).unwrap();
        assert_eq!(cli.command.verbosity(), Verbosity::Normal);
    }

    #[test]
    fn up_verbose_verbosity() {
        use crate::progress::Verbosity;
        let cli = parse(&["cella", "up", "--verbose"]).unwrap();
        assert_eq!(cli.command.verbosity(), Verbosity::Verbose);
    }

    #[test]
    fn list_verbosity_is_normal() {
        use crate::progress::Verbosity;
        let cli = parse(&["cella", "list"]).unwrap();
        assert_eq!(cli.command.verbosity(), Verbosity::Normal);
    }

    #[test]
    fn up_is_not_daemon_start() {
        let cli = parse(&["cella", "up"]).unwrap();
        assert!(!cli.command.is_daemon_start());
    }

    // ── no subcommand ───────────────────────────────────────────────

    #[test]
    fn parse_no_subcommand_is_error() {
        let result = parse(&["cella"]);
        assert!(result.is_err());
    }

    // ── unknown subcommand ──────────────────────────────────────────

    #[test]
    fn parse_unknown_subcommand_is_error() {
        let result = parse(&["cella", "nonexistent"]);
        assert!(result.is_err());
    }

    // ── docker-host flag on commands that support it ────────────────

    #[test]
    fn parse_exec_with_docker_host() {
        let cli = parse(&[
            "cella",
            "exec",
            "--docker-host",
            "tcp://localhost:2375",
            "ls",
        ])
        .unwrap();
        assert!(matches!(cli.command, super::commands::Command::Exec(_)));
    }

    #[test]
    fn parse_exec_with_workdir() {
        let cli = parse(&["cella", "exec", "--workdir", "/app", "ls"]).unwrap();
        assert!(matches!(cli.command, super::commands::Command::Exec(_)));
    }

    #[test]
    fn parse_exec_with_container_id() {
        let cli = parse(&["cella", "exec", "--container-id", "abc123", "ls"]).unwrap();
        assert!(matches!(cli.command, super::commands::Command::Exec(_)));
    }

    #[test]
    fn parse_exec_with_container_name() {
        let cli = parse(&["cella", "exec", "--container-name", "mycontainer", "ls"]).unwrap();
        assert!(matches!(cli.command, super::commands::Command::Exec(_)));
    }

    #[test]
    fn parse_down_with_branch() {
        let cli = parse(&["cella", "down", "--branch", "feat/x"]).unwrap();
        assert!(matches!(cli.command, super::commands::Command::Down(_)));
    }

    #[test]
    fn parse_down_container_id_conflicts_with_branch() {
        let result = parse(&[
            "cella",
            "down",
            "--container-id",
            "abc123",
            "--branch",
            "feat/x",
        ]);
        assert!(result.is_err());
    }

    #[test]
    fn parse_down_container_name_conflicts_with_branch() {
        let result = parse(&[
            "cella",
            "down",
            "--container-name",
            "mycontainer",
            "--branch",
            "feat/x",
        ]);
        assert!(result.is_err());
    }

    // ── shell command ───────────────────────────────────────────────

    #[test]
    fn parse_shell_minimal() {
        let cli = parse(&["cella", "shell"]).unwrap();
        assert!(matches!(cli.command, super::commands::Command::Shell(_)));
    }

    // ── logs command ────────────────────────────────────────────────

    #[test]
    fn parse_logs_minimal() {
        let cli = parse(&["cella", "logs"]).unwrap();
        assert!(matches!(cli.command, super::commands::Command::Logs(_)));
    }

    // ── init command ────────────────────────────────────────────────

    #[test]
    fn parse_init_minimal() {
        let cli = parse(&["cella", "init"]).unwrap();
        assert!(matches!(cli.command, super::commands::Command::Init(_)));
    }

    // ── config command ──────────────────────────────────────────────

    #[test]
    fn parse_config_requires_subcommand() {
        let result = parse(&["cella", "config"]);
        assert!(result.is_err());
    }

    #[test]
    fn parse_config_show() {
        let cli = parse(&["cella", "config", "show"]).unwrap();
        assert!(matches!(cli.command, super::commands::Command::Config(_)));
    }

    // ── templates command ───────────────────────────────────────────

    #[test]
    fn parse_templates_requires_subcommand() {
        let result = parse(&["cella", "templates"]);
        assert!(result.is_err());
    }

    #[test]
    fn parse_templates_list() {
        let cli = parse(&["cella", "templates", "list"]).unwrap();
        assert!(matches!(
            cli.command,
            super::commands::Command::Templates(_)
        ));
    }

    #[test]
    fn parse_template_singular_alias_list() {
        // `cella template list` (singular) must route to the same Templates variant.
        let cli = parse(&["cella", "template", "list"]).unwrap();
        assert!(matches!(
            cli.command,
            super::commands::Command::Templates(_)
        ));
    }

    #[test]
    fn parse_templates_apply_requires_template_id() {
        let result = parse(&["cella", "templates", "apply"]);
        assert!(result.is_err());
    }

    #[test]
    fn parse_templates_apply_with_template_id() {
        let cli = parse(&[
            "cella",
            "templates",
            "apply",
            "--template-id",
            "ghcr.io/devcontainers/templates/rust",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            super::commands::Command::Templates(_)
        ));
    }

    #[test]
    fn parse_templates_apply_short_flags() {
        let cli = parse(&[
            "cella",
            "templates",
            "apply",
            "-t",
            "ghcr.io/devcontainers/templates/rust",
            "-w",
            "/tmp/ws",
            "-a",
            "{}",
            "-f",
            "[]",
        ])
        .unwrap();
        if let super::commands::Command::Templates(args) = cli.command {
            if let super::commands::templates::TemplatesCommand::Apply(apply_args) = args.command {
                assert_eq!(
                    apply_args.template_id,
                    "ghcr.io/devcontainers/templates/rust"
                );
            } else {
                panic!("expected Apply subcommand");
            }
        } else {
            panic!("expected Templates command");
        }
    }

    #[test]
    fn templates_apply_is_not_text_output() {
        let cli = parse(&[
            "cella",
            "templates",
            "apply",
            "-t",
            "ghcr.io/devcontainers/templates/rust",
        ])
        .unwrap();
        assert!(!cli.command.is_text_output());
    }

    // ── ports command ───────────────────────────────────────────────

    #[test]
    fn parse_ports_minimal() {
        let cli = parse(&["cella", "ports"]).unwrap();
        assert!(matches!(cli.command, super::commands::Command::Ports(_)));
    }

    // ── credential command ──────────────────────────────────────────

    #[test]
    fn parse_credential_requires_subcommand() {
        let result = parse(&["cella", "credential"]);
        assert!(result.is_err());
    }

    // ── read-configuration command ──────────────────────────────────

    #[test]
    fn parse_read_configuration_minimal() {
        let cli = parse(&["cella", "read-configuration"]).unwrap();
        assert!(matches!(
            cli.command,
            super::commands::Command::ReadConfiguration(_)
        ));
    }

    // ── switch command ──────────────────────────────────────────────

    #[test]
    fn parse_switch_without_name_uses_picker() {
        let cli = parse(&["cella", "switch"]).unwrap();
        assert!(matches!(cli.command, super::commands::Command::Switch(_)));
    }

    #[test]
    fn parse_switch_with_name() {
        let cli = parse(&["cella", "switch", "feat/new"]).unwrap();
        assert!(matches!(cli.command, super::commands::Command::Switch(_)));
    }

    // ── log-level / log-format accessors ────────────────────────────

    #[test]
    fn up_log_level_accessor() {
        use super::commands::LogLevel;
        let cli = parse(&["cella", "up", "--log-level", "trace"]).unwrap();
        assert!(matches!(cli.command.log_level(), Some(LogLevel::Trace)));
    }

    #[test]
    fn up_without_log_level_is_none() {
        let cli = parse(&["cella", "up"]).unwrap();
        assert!(cli.command.log_level().is_none());
    }

    #[test]
    fn non_up_command_log_level_is_none() {
        // Commands that don't carry --log-level return None.
        let cli = parse(&["cella", "list"]).unwrap();
        assert!(cli.command.log_level().is_none());
    }

    #[test]
    fn templates_apply_log_level_wired() {
        use super::commands::LogLevel;
        let cli = parse(&[
            "cella",
            "templates",
            "apply",
            "-t",
            "ghcr.io/devcontainers/templates/rust",
            "--log-level",
            "debug",
        ])
        .unwrap();
        assert!(matches!(cli.command.log_level(), Some(LogLevel::Debug)));
    }

    #[test]
    fn templates_apply_default_log_level_is_info() {
        use super::commands::LogLevel;
        let cli = parse(&[
            "cella",
            "templates",
            "apply",
            "-t",
            "ghcr.io/devcontainers/templates/rust",
        ])
        .unwrap();
        assert!(matches!(cli.command.log_level(), Some(LogLevel::Info)));
    }

    #[test]
    fn up_log_format_json_accessor() {
        use super::commands::LogFormat;
        let cli = parse(&["cella", "up", "--log-format", "json"]).unwrap();
        assert!(matches!(cli.command.log_format(), LogFormat::Json));
    }

    #[test]
    fn up_default_log_format_is_text() {
        use super::commands::LogFormat;
        let cli = parse(&["cella", "up"]).unwrap();
        assert!(matches!(cli.command.log_format(), LogFormat::Text));
    }

    #[test]
    fn non_up_command_log_format_is_text() {
        use super::commands::LogFormat;
        let cli = parse(&["cella", "list"]).unwrap();
        assert!(matches!(cli.command.log_format(), LogFormat::Text));
    }

    #[test]
    fn code_inherits_up_log_flags() {
        use super::commands::{LogFormat, LogLevel};
        let cli = parse(&[
            "cella",
            "code",
            "--log-level",
            "debug",
            "--log-format",
            "json",
        ])
        .unwrap();
        assert!(matches!(cli.command.log_level(), Some(LogLevel::Debug)));
        assert!(matches!(cli.command.log_format(), LogFormat::Json));
    }

    // ── terminal-columns / terminal-rows pairing ────────────────────

    #[test]
    fn parse_up_terminal_columns_alone_is_error() {
        // clap `requires` enforces the official both-required pairing.
        let result = parse(&["cella", "up", "--terminal-columns", "80"]);
        assert!(result.is_err());
    }

    #[test]
    fn parse_up_terminal_rows_alone_is_error() {
        let result = parse(&["cella", "up", "--terminal-rows", "24"]);
        assert!(result.is_err());
    }

    #[test]
    fn parse_up_terminal_pair_is_ok() {
        let cli = parse(&[
            "cella",
            "up",
            "--terminal-columns",
            "80",
            "--terminal-rows",
            "24",
        ])
        .unwrap();
        assert!(matches!(cli.command, super::commands::Command::Up(_)));
    }

    // ── omit-syntax-directive (hidden, parseable no-op) ─────────────

    #[test]
    fn parse_up_omit_syntax_directive_is_ok() {
        let cli = parse(&["cella", "up", "--omit-syntax-directive"]).unwrap();
        assert!(matches!(cli.command, super::commands::Command::Up(_)));
    }

    // ── omit-config-remote-env-from-metadata (hidden, parseable) ────

    #[test]
    fn parse_up_omit_config_remote_env_from_metadata_is_ok() {
        let cli = parse(&["cella", "up", "--omit-config-remote-env-from-metadata"]).unwrap();
        assert!(matches!(cli.command, super::commands::Command::Up(_)));
    }

    // ── features info --log-level wiring ────────────────────────────

    #[test]
    fn features_info_log_level_feeds_global_filter() {
        let cli = parse(&[
            "cella",
            "features",
            "info",
            "dependencies",
            "ghcr.io/devcontainers/features/node:1",
            "--log-level",
            "debug",
        ])
        .unwrap();
        assert!(
            matches!(
                cli.command.log_level(),
                Some(super::commands::LogLevel::Debug)
            ),
            "features info --log-level must reach Command::log_level()"
        );
    }
}

// ── log directive / spinner predicate helpers ───────────────────────

#[cfg(test)]
mod log_init_tests {
    use super::{LogFormat, LogLevel, resolve_log_directive, spinners_enabled};

    #[test]
    fn directive_default_is_info() {
        // No RUST_LOG, no --log-level => plain `info`.
        assert_eq!(resolve_log_directive(false, None).as_deref(), Some("info"));
    }

    #[test]
    fn directive_explicit_info() {
        assert_eq!(
            resolve_log_directive(false, Some(LogLevel::Info)).as_deref(),
            Some("info")
        );
    }

    #[test]
    fn directive_debug_scopes_to_cella() {
        assert_eq!(
            resolve_log_directive(false, Some(LogLevel::Debug)).as_deref(),
            Some("cella=debug,info")
        );
    }

    #[test]
    fn directive_trace_scopes_to_cella() {
        assert_eq!(
            resolve_log_directive(false, Some(LogLevel::Trace)).as_deref(),
            Some("cella=trace,info")
        );
    }

    #[test]
    fn rust_log_wins_over_log_level() {
        // When RUST_LOG is set, the directive is None so the caller falls back
        // to EnvFilter::from_default_env() — RUST_LOG wins untouched.
        assert!(resolve_log_directive(true, Some(LogLevel::Trace)).is_none());
        assert!(resolve_log_directive(true, None).is_none());
    }

    #[test]
    fn spinners_off_under_json_even_on_tty() {
        // Json log-format disables spinners even on a TTY with no RUST_LOG.
        assert!(!spinners_enabled(true, false, true, LogFormat::Json));
    }

    #[test]
    fn spinners_on_for_text_tty_no_rust_log() {
        assert!(spinners_enabled(true, false, true, LogFormat::Text));
    }

    #[test]
    fn spinners_off_when_rust_log_set() {
        assert!(!spinners_enabled(true, true, true, LogFormat::Text));
    }

    #[test]
    fn spinners_off_when_not_tty() {
        assert!(!spinners_enabled(true, false, false, LogFormat::Text));
    }

    #[test]
    fn spinners_off_when_not_text_output() {
        assert!(!spinners_enabled(false, false, true, LogFormat::Text));
    }

    // ── runtime effect of the resolved directive ────────────────────
    //
    // String-equality on the directive is not enough: what matters is which
    // events the resulting `EnvFilter` actually lets through. These tests feed
    // the real directive into a capture subscriber and assert the runtime
    // effect against explicit event targets — so the `cella`-scoping decision
    // is verified, not assumed.

    use std::io::Write;
    use std::sync::{Arc, Mutex};

    use tracing_subscriber::EnvFilter;

    #[derive(Clone, Default)]
    struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for CaptureWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CaptureWriter {
        type Writer = Self;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Emit a set of explicit-target debug events under the given directive and
    /// return the captured stderr text.
    fn capture_under_directive(directive: &str) -> String {
        let buf = CaptureWriter::default();
        let sink = buf.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::new(directive))
            .with_writer(buf)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            tracing::debug!(target: "cella", "binary-crate event");
            tracing::debug!(target: "cella::commands::up", "binary-module event");
            tracing::debug!(target: "cella_orchestrator::up", "library-crate event");
            tracing::debug!(target: "cella_features::lib", "library-crate event 2");
            tracing::info!(target: "cella_orchestrator::up", "library info event");
            tracing::debug!(target: "h2::codec", "dependency event");
        });
        String::from_utf8(sink.0.lock().unwrap().clone()).unwrap()
    }

    #[test]
    fn directive_debug_enables_all_cella_crates_not_deps() {
        // The resolved `cella=debug,info` directive must raise EVERY cella crate
        // (binary `cella` AND library `cella_*`) to debug, while keeping noisy
        // dependency crates at the `info` floor (so their debug logs are hidden).
        let directive = resolve_log_directive(false, Some(LogLevel::Debug)).unwrap();
        let out = capture_under_directive(&directive);

        assert!(
            out.contains("binary-crate event"),
            "binary crate missed: {out}"
        );
        assert!(
            out.contains("library-crate event"),
            "cella_orchestrator debug missed (scoping bug): {out}"
        );
        assert!(
            out.contains("library-crate event 2"),
            "cella_features debug missed (scoping bug): {out}"
        );
        // Dependency debug is suppressed by the `info` global floor.
        assert!(
            !out.contains("dependency event"),
            "dependency debug should be hidden at info floor: {out}"
        );
    }

    #[test]
    fn directive_default_info_hides_all_debug() {
        // Plain `info` must hide every debug event, cella or otherwise, while
        // still letting info-level cella events through.
        let directive = resolve_log_directive(false, None).unwrap();
        let out = capture_under_directive(&directive);

        assert!(
            !out.contains("binary-crate event"),
            "info leaked debug: {out}"
        );
        assert!(
            !out.contains("library-crate event"),
            "info leaked debug: {out}"
        );
        assert!(
            out.contains("library info event"),
            "info event missed: {out}"
        );
    }
}
