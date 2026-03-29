//! In-container CLI mode for cella.
//!
//! When the agent binary is invoked as `cella` (via symlink at `/cella/bin/cella`),
//! it enters CLI mode instead of daemon mode. CLI commands delegate to the host
//! daemon via the existing TCP control connection.

use cella_port::protocol::{AgentMessage, DaemonMessage, OutputStream, WorktreeOperationResult};

use crate::control::ControlClient;

/// In-container CLI commands.
pub enum CliCommand {
    Branch { name: String, base: Option<String> },
    List,
    Help,
    Unsupported { command: String },
}

/// Parse CLI arguments for in-container mode.
pub fn parse_cli_args(args: &[String]) -> CliCommand {
    let subcmd = args.get(1).map(String::as_str);

    match subcmd {
        Some("branch") => {
            let name = match args.get(2) {
                Some(n) if !n.starts_with('-') => n.clone(),
                _ => {
                    return CliCommand::Help;
                }
            };
            let mut base = None;
            let mut i = 3;
            while i < args.len() {
                if args[i] == "--base" {
                    base = args.get(i + 1).cloned();
                    i += 2;
                } else {
                    i += 1;
                }
            }
            CliCommand::Branch { name, base }
        }
        Some("list" | "ls") => CliCommand::List,
        Some("--help" | "-h") | None => CliCommand::Help,
        Some(cmd) => {
            let supported = ["branch", "list", "ls"];
            if supported.contains(&cmd) {
                CliCommand::Help
            } else {
                CliCommand::Unsupported {
                    command: cmd.to_string(),
                }
            }
        }
    }
}

/// Run the in-container CLI.
pub async fn run(command: CliCommand) -> Result<(), Box<dyn std::error::Error>> {
    match command {
        CliCommand::Help => {
            print_help();
            Ok(())
        }
        CliCommand::Unsupported { command } => {
            print_unsupported(&command);
            std::process::exit(1);
        }
        CliCommand::Branch { name, base } => run_branch(&name, base.as_deref()).await,
        CliCommand::List => run_list().await,
    }
}

fn print_help() {
    eprintln!(
        "\
cella — dev container worktree management (in-container)

Usage: cella <command> [options]

Commands:
  branch <name> [--base ref]   Create a worktree-backed branch with its own container
  list                         List worktree branches and their containers

Options:
  --help, -h                   Show this help message

Run `cella --help` on the host for all commands."
    );
}

fn print_unsupported(command: &str) {
    eprintln!(
        "\
Error: `cella {command}` is not available inside a dev container.

Available commands inside containers:
  cella branch <name>   Create a worktree-backed branch
  cella list            List worktree branches

Run `cella --help` on the host for all commands."
    );
}

/// Connect to the host daemon for CLI commands.
async fn connect_daemon() -> Result<ControlClient, Box<dyn std::error::Error>> {
    let addr = std::env::var("CELLA_DAEMON_ADDR")
        .map_err(|_| "CELLA_DAEMON_ADDR not set. Are you inside a cella dev container?")?;
    let token = std::env::var("CELLA_DAEMON_TOKEN").unwrap_or_default();
    let container_name = std::env::var("CELLA_CONTAINER_NAME").unwrap_or_default();

    let (client, _hello) = ControlClient::connect(&addr, &container_name, &token)
        .await
        .map_err(|e| format!("Failed to connect to host daemon: {e}"))?;

    Ok(client)
}

/// Generate a unique request ID.
fn request_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("cli-{n}")
}

async fn run_branch(name: &str, base: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    let mut client = connect_daemon().await?;

    let id = request_id();
    let msg = AgentMessage::BranchRequest {
        request_id: id.clone(),
        branch: name.to_string(),
        base: base.map(String::from),
    };
    client.send(&msg).await?;

    // Read responses until we get the final BranchResult.
    loop {
        let resp = client.recv().await?;
        match resp {
            DaemonMessage::OperationProgress { step, message, .. } => {
                eprintln!("\u{25cf} {step}: {message}");
            }
            DaemonMessage::OperationOutput { stream, data, .. } => match stream {
                OutputStream::Stdout => print!("{data}"),
                OutputStream::Stderr => eprint!("{data}"),
            },
            DaemonMessage::BranchResult { result, .. } => match result {
                WorktreeOperationResult::Success {
                    container_name,
                    worktree_path,
                } => {
                    eprintln!("Ready: {worktree_path} (container: {container_name})");
                    return Ok(());
                }
                WorktreeOperationResult::Error { message } => {
                    return Err(message.into());
                }
            },
            _ => {
                // Ignore unrelated messages (Ack, PortMapping, etc.)
            }
        }
    }
}

async fn run_list() -> Result<(), Box<dyn std::error::Error>> {
    let mut client = connect_daemon().await?;

    let id = request_id();
    let msg = AgentMessage::ListRequest {
        request_id: id.clone(),
    };
    client.send(&msg).await?;

    loop {
        let resp = client.recv().await?;
        if let DaemonMessage::ListResult { worktrees, .. } = resp {
            if worktrees.is_empty() {
                eprintln!("No worktree branches found.");
            } else {
                print_worktree_table(&worktrees);
            }
            return Ok(());
        }
        // Ignore unrelated messages (Ack, PortMapping, etc.)
    }
}

fn print_worktree_table(worktrees: &[cella_port::protocol::WorktreeEntry]) {
    const HEADER: &str = "BRANCH               STATE      CONTAINER                      PATH";
    println!("{HEADER}");
    for wt in worktrees {
        let branch = wt.branch.as_deref().unwrap_or("(detached)");
        let state = wt.container_state.as_deref().unwrap_or("-");
        let container = wt.container_name.as_deref().unwrap_or("-");
        let marker = if wt.is_main { " *" } else { "" };
        println!(
            "{:<20} {:<10} {:<30} {}",
            format!("{branch}{marker}"),
            state,
            container,
            wt.worktree_path,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_branch_command() {
        let args = vec![
            "cella".to_string(),
            "branch".to_string(),
            "feat/auth".to_string(),
        ];
        let cmd = parse_cli_args(&args);
        assert!(
            matches!(cmd, CliCommand::Branch { name, base } if name == "feat/auth" && base.is_none())
        );
    }

    #[test]
    fn parse_branch_with_base() {
        let args = vec![
            "cella".to_string(),
            "branch".to_string(),
            "feat/auth".to_string(),
            "--base".to_string(),
            "main".to_string(),
        ];
        let cmd = parse_cli_args(&args);
        assert!(matches!(cmd, CliCommand::Branch { name, base }
            if name == "feat/auth" && base.as_deref() == Some("main")));
    }

    #[test]
    fn parse_list_command() {
        let args = vec!["cella".to_string(), "list".to_string()];
        let cmd = parse_cli_args(&args);
        assert!(matches!(cmd, CliCommand::List));
    }

    #[test]
    fn parse_ls_alias() {
        let args = vec!["cella".to_string(), "ls".to_string()];
        let cmd = parse_cli_args(&args);
        assert!(matches!(cmd, CliCommand::List));
    }

    #[test]
    fn parse_help() {
        let args = vec!["cella".to_string(), "--help".to_string()];
        let cmd = parse_cli_args(&args);
        assert!(matches!(cmd, CliCommand::Help));
    }

    #[test]
    fn parse_no_args() {
        let args = vec!["cella".to_string()];
        let cmd = parse_cli_args(&args);
        assert!(matches!(cmd, CliCommand::Help));
    }

    #[test]
    fn parse_unsupported_command() {
        let args = vec!["cella".to_string(), "up".to_string()];
        let cmd = parse_cli_args(&args);
        assert!(matches!(cmd, CliCommand::Unsupported { command } if command == "up"));
    }

    #[test]
    fn parse_branch_missing_name_shows_help() {
        let args = vec!["cella".to_string(), "branch".to_string()];
        let cmd = parse_cli_args(&args);
        assert!(matches!(cmd, CliCommand::Help));
    }
}
