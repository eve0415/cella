pub mod backend;
mod commands;
pub mod picker;
pub mod progress;
pub mod style;
mod table;

use std::io::IsTerminal;

use clap::Parser;
use tracing_subscriber::EnvFilter;

use progress::{IndicatifMakeWriter, Progress};

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
async fn main() -> Result<(), Box<dyn std::error::Error>> {
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

    // Spinners are active when: text output mode AND no RUST_LOG AND stderr is a TTY.
    let rust_log_set = std::env::var_os("RUST_LOG").is_some();
    let is_tty = std::io::stderr().is_terminal();
    let spinners_enabled = cli.command.is_text_output() && !rust_log_set && is_tty;

    let progress = Progress::new(spinners_enabled, verbosity);

    // The daemon subprocess initializes its own file-based tracing.
    // Skip the normal indicatif-based tracing for daemon start.
    if !cli.command.is_daemon_start() {
        if spinners_enabled {
            // Route tracing through indicatif so log lines don't corrupt spinners.
            tracing_subscriber::fmt()
                .with_env_filter(EnvFilter::from_default_env())
                .with_writer(IndicatifMakeWriter::new(progress.multi().clone()))
                .init();
        } else {
            // No spinners (JSON mode, RUST_LOG set, non-TTY): write directly to stderr.
            // Spec requires stdout = JSON only, stderr = logs only.
            tracing_subscriber::fmt()
                .with_env_filter(EnvFilter::from_default_env())
                .with_writer(std::io::stderr)
                .init();
        }
    }

    cli.command.execute(progress).await
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
    fn parse_list_json() {
        let cli = parse(&["cella", "list", "--json"]).unwrap();
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
    fn parse_doctor_json() {
        let cli = parse(&["cella", "doctor", "--json"]).unwrap();
        assert!(matches!(cli.command, super::commands::Command::Doctor(_)));
    }

    #[test]
    fn parse_doctor_no_redact() {
        let cli = parse(&["cella", "doctor", "--no-redact"]).unwrap();
        assert!(matches!(cli.command, super::commands::Command::Doctor(_)));
    }

    #[test]
    fn parse_doctor_all_flags() {
        let cli = parse(&["cella", "doctor", "--all", "--json", "--no-redact"]).unwrap();
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

    // ── code command ────────────────────────────────────────────────

    #[test]
    fn parse_code_minimal() {
        let cli = parse(&["cella", "code"]).unwrap();
        assert!(matches!(cli.command, super::commands::Command::Code(_)));
    }

    #[test]
    fn parse_code_insider() {
        let cli = parse(&["cella", "code", "--insider"]).unwrap();
        if let super::commands::Command::Code(args) = cli.command {
            assert!(args.insider);
        } else {
            panic!("expected Code command");
        }
    }

    #[test]
    fn parse_code_cursor() {
        let cli = parse(&["cella", "code", "--cursor"]).unwrap();
        if let super::commands::Command::Code(args) = cli.command {
            assert!(args.cursor);
        } else {
            panic!("expected Code command");
        }
    }

    #[test]
    fn parse_code_insider_and_cursor_conflict() {
        let result = parse(&["cella", "code", "--insider", "--cursor"]);
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
    fn parse_code_binary_and_insider_conflict() {
        let result = parse(&["cella", "code", "--binary", "x", "--insider"]);
        assert!(result.is_err());
    }

    // ── tmux command ────────────────────────────────────────────────

    #[test]
    fn parse_tmux_minimal() {
        let cli = parse(&["cella", "tmux"]).unwrap();
        assert!(matches!(cli.command, super::commands::Command::Tmux(_)));
    }

    #[test]
    fn parse_tmux_force() {
        let cli = parse(&["cella", "tmux", "--force"]).unwrap();
        if let super::commands::Command::Tmux(args) = cli.command {
            assert!(args.force);
        } else {
            panic!("expected Tmux command");
        }
    }

    #[test]
    fn parse_tmux_with_extra_args() {
        let cli = parse(&["cella", "tmux", "--", "list-sessions"]).unwrap();
        if let super::commands::Command::Tmux(args) = cli.command {
            assert_eq!(args.extra_args, vec!["list-sessions"]);
        } else {
            panic!("expected Tmux command");
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
    fn up_default_is_text_output() {
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

    // ── template command ────────────────────────────────────────────

    #[test]
    fn parse_template_requires_subcommand() {
        let result = parse(&["cella", "template"]);
        assert!(result.is_err());
    }

    #[test]
    fn parse_template_list() {
        let cli = parse(&["cella", "template", "list"]).unwrap();
        assert!(matches!(cli.command, super::commands::Command::Template(_)));
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
}
