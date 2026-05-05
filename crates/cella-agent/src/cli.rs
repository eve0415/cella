//! In-container CLI mode for cella.
//!
//! When the agent binary is invoked as `cella` (via symlink at `/cella/bin/cella`),
//! it enters CLI mode instead of daemon mode. CLI commands delegate to the host
//! daemon via the existing TCP control connection.

use std::time::Duration;

use cella_protocol::{AgentMessage, DaemonMessage, OutputStream, WorktreeOperationResult};
use tokio::time::timeout;

use crate::control::ControlClient;

/// Timeout for fast operations (list, task list, task stop).
const TIMEOUT_FAST: Duration = Duration::from_secs(30);
/// Timeout for medium operations (down, exec, prune, task logs/wait).
const TIMEOUT_MEDIUM: Duration = Duration::from_mins(2);
/// Timeout for slow operations (branch, up, task run).
const TIMEOUT_SLOW: Duration = Duration::from_mins(10);

/// Receive a message with a timeout that resets on each message in the loop.
async fn recv_timeout(
    client: &mut ControlClient,
    dur: Duration,
) -> Result<DaemonMessage, Box<dyn std::error::Error + Send + Sync>> {
    Ok(timeout(dur, client.recv()).await.map_err(
        |_| -> Box<dyn std::error::Error + Send + Sync> {
            "timed out waiting for response from daemon".into()
        },
    )??)
}

/// In-container CLI commands.
pub enum CliCommand {
    Branch {
        name: String,
        base: Option<String>,
        labels: Vec<String>,
    },
    List {
        json: bool,
    },
    Down {
        branch: String,
        rm: bool,
        volumes: bool,
        force: bool,
    },
    Up {
        branch: String,
        rebuild: bool,
    },
    Exec {
        branch: String,
        command: Vec<String>,
        json: bool,
    },
    Prune {
        dry_run: bool,
        all: bool,
        older_than: Option<String>,
        missing_worktree: bool,
        labels: Vec<String>,
    },
    TaskRun {
        branch: String,
        command: Vec<String>,
        base: Option<String>,
    },
    TaskList {
        json: bool,
    },
    TaskLogs {
        branch: String,
        follow: bool,
    },
    TaskWait {
        branch: String,
    },
    TaskStop {
        branch: String,
    },
    Switch {
        branch: String,
    },
    Doctor {
        json: bool,
    },
    Help,
    CommandHelp,
    Unsupported {
        command: String,
    },
}

/// Parse CLI arguments for in-container mode.
pub fn parse_cli_args(args: &[String]) -> CliCommand {
    let subcmd = args.get(1).map(String::as_str);

    if let Some(cmd) = subcmd.filter(|c| {
        *c != "--help" && *c != "-h" && args[2..].iter().any(|a| a == "--help" || a == "-h")
    }) {
        print_command_help(cmd);
        return CliCommand::CommandHelp;
    }

    match subcmd {
        Some("branch") => parse_branch_subcommand(args),
        Some("list" | "ls") => {
            let json = args[2..].iter().any(|a| a == "--json");
            CliCommand::List { json }
        }
        Some("exec") => {
            // Parse: cella exec <branch> [--json] -- <cmd...>
            let branch = match args.get(2) {
                Some(b) if !b.starts_with('-') && b != "--" => b.clone(),
                _ => return CliCommand::Help,
            };
            let sep = args.iter().position(|a| a == "--");
            let command = sep.map_or_else(Vec::new, |i| args[i + 1..].to_vec());
            if command.is_empty() {
                return CliCommand::Help;
            }
            let json = args[3..sep.unwrap_or(args.len())]
                .iter()
                .any(|a| a == "--json");
            CliCommand::Exec {
                branch,
                command,
                json,
            }
        }
        Some("down") => {
            let branch = match args.get(2) {
                Some(b) if !b.starts_with('-') => b.clone(),
                _ => return CliCommand::Help,
            };
            let mut rm = false;
            let mut volumes = false;
            let mut force = false;
            for arg in &args[3..] {
                match arg.as_str() {
                    "--rm" => rm = true,
                    "--volumes" => volumes = true,
                    "--force" => force = true,
                    f if f.starts_with('-') => {
                        eprintln!("Error: unknown flag '{f}' for down command");
                        return CliCommand::Help;
                    }
                    _ => {}
                }
            }
            if volumes && !rm {
                eprintln!("Error: --volumes requires --rm");
                return CliCommand::Help;
            }
            CliCommand::Down {
                branch,
                rm,
                volumes,
                force,
            }
        }
        Some("up") => {
            let branch = match args.get(2) {
                Some(b) if !b.starts_with('-') => b.clone(),
                _ => return CliCommand::Help,
            };
            let mut rebuild = false;
            for arg in &args[3..] {
                match arg.as_str() {
                    "--rebuild" => rebuild = true,
                    f if f.starts_with('-') => {
                        eprintln!("Error: unknown flag '{f}' for up command");
                        return CliCommand::Help;
                    }
                    _ => {}
                }
            }
            CliCommand::Up { branch, rebuild }
        }
        Some("prune") => parse_prune_subcommand(args),
        Some("task") => parse_task_subcommand(args),
        Some("switch") => {
            let branch = match args.get(2) {
                Some(b) if !b.starts_with('-') => b.clone(),
                _ => return CliCommand::Help,
            };
            CliCommand::Switch { branch }
        }
        Some("doctor") => {
            let json = args[2..].iter().any(|a| a == "--json");
            CliCommand::Doctor { json }
        }
        Some("--help" | "-h") | None => CliCommand::Help,
        Some(cmd) => CliCommand::Unsupported {
            command: cmd.to_string(),
        },
    }
}

fn parse_branch_subcommand(args: &[String]) -> CliCommand {
    let name = match args.get(2) {
        Some(n) if !n.starts_with('-') => n.clone(),
        _ => return CliCommand::Help,
    };
    if name.is_empty() {
        eprintln!("Error: branch name cannot be empty");
        return CliCommand::Help;
    }
    if name.contains(|c: char| c.is_whitespace()) {
        eprintln!("Error: branch name cannot contain whitespace");
        return CliCommand::Help;
    }
    let mut base = None;
    let mut labels = Vec::new();
    let mut i = 3;
    while i < args.len() {
        if args[i] == "--base" {
            match args.get(i + 1) {
                Some(val) if !val.starts_with('-') => {
                    base = Some(val.clone());
                    i += 2;
                }
                _ => {
                    eprintln!("Error: --base requires a value (e.g., --base main)");
                    return CliCommand::Help;
                }
            }
        } else if args[i] == "--label" {
            match args.get(i + 1) {
                Some(val) if val.contains('=') => {
                    if val.starts_with("dev.cella.") || val.starts_with("devcontainer.") {
                        eprintln!(
                            "Error: reserved label prefix in '{val}' (dev.cella.* and devcontainer.* are reserved)"
                        );
                        return CliCommand::Help;
                    }
                    labels.push(val.clone());
                    i += 2;
                }
                _ => {
                    eprintln!("Error: --label requires KEY=VALUE format");
                    return CliCommand::Help;
                }
            }
        } else if args[i].starts_with('-') {
            eprintln!("Error: unknown flag '{}' for branch command", args[i]);
            return CliCommand::Help;
        } else {
            i += 1;
        }
    }
    CliCommand::Branch { name, base, labels }
}

fn parse_prune_subcommand(args: &[String]) -> CliCommand {
    let mut dry_run = false;
    let mut all = false;
    let mut older_than = None;
    let mut missing_worktree = false;
    let mut labels = Vec::new();
    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--dry-run" => {
                dry_run = true;
                i += 1;
            }
            "--all" => {
                all = true;
                i += 1;
            }
            "--missing-worktree" => {
                missing_worktree = true;
                i += 1;
            }
            "--older-than" => match args.get(i + 1) {
                Some(val) if !val.starts_with('-') => {
                    older_than = Some(val.clone());
                    i += 2;
                }
                _ => {
                    eprintln!("Error: --older-than requires a value (e.g., --older-than 7d)");
                    return CliCommand::Help;
                }
            },
            "--label" => match args.get(i + 1) {
                Some(val) if val.contains('=') => {
                    labels.push(val.clone());
                    i += 2;
                }
                _ => {
                    eprintln!("Error: --label requires KEY=VALUE format");
                    return CliCommand::Help;
                }
            },
            f if f.starts_with('-') => {
                eprintln!("Error: unknown flag '{f}' for prune command");
                return CliCommand::Help;
            }
            _ => {
                i += 1;
            }
        }
    }
    CliCommand::Prune {
        dry_run,
        all,
        older_than,
        missing_worktree,
        labels,
    }
}

fn parse_task_subcommand(args: &[String]) -> CliCommand {
    let sub = args.get(2).map(String::as_str);
    match sub {
        Some("run") => {
            let branch = match args.get(3) {
                Some(b) if !b.starts_with('-') && b != "--" => b.clone(),
                _ => return CliCommand::Help,
            };
            let sep = args.iter().position(|a| a == "--");
            let command = sep.map_or_else(Vec::new, |i| args[i + 1..].to_vec());
            if command.is_empty() {
                return CliCommand::Help;
            }
            // Check for --base before the separator
            let mut base = None;
            let end = sep.unwrap_or(args.len());
            let mut i = 4;
            while i < end {
                if args[i] == "--base" {
                    match args.get(i + 1) {
                        Some(val) if !val.starts_with('-') => {
                            base = Some(val.clone());
                            i += 2;
                        }
                        _ => {
                            eprintln!("Error: --base requires a value (e.g., --base main)");
                            return CliCommand::Help;
                        }
                    }
                } else if args[i].starts_with('-') {
                    eprintln!("Error: unknown flag '{}' for task run command", args[i]);
                    return CliCommand::Help;
                } else {
                    i += 1;
                }
            }
            CliCommand::TaskRun {
                branch,
                command,
                base,
            }
        }
        Some("list" | "ls") => {
            let json = args[3..].iter().any(|a| a == "--json");
            CliCommand::TaskList { json }
        }
        Some("logs") => {
            // Parse: cella task logs [-f|--follow] <branch>
            let follow = args[3..].iter().any(|a| a == "-f" || a == "--follow");
            let branch = args[3..].iter().find(|a| !a.starts_with('-')).cloned();
            branch.map_or(CliCommand::Help, |b| CliCommand::TaskLogs {
                branch: b,
                follow,
            })
        }
        Some("wait") => {
            let branch = match args.get(3) {
                Some(b) if !b.starts_with('-') => b.clone(),
                _ => return CliCommand::Help,
            };
            CliCommand::TaskWait { branch }
        }
        Some("stop") => {
            let branch = match args.get(3) {
                Some(b) if !b.starts_with('-') => b.clone(),
                _ => return CliCommand::Help,
            };
            CliCommand::TaskStop { branch }
        }
        _ => CliCommand::Help,
    }
}

/// Run the in-container CLI.
pub async fn run(command: CliCommand) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    match command {
        CliCommand::Help => {
            print_help();
            Ok(())
        }
        CliCommand::CommandHelp => Ok(()),
        CliCommand::Doctor { json } => {
            if json {
                run_doctor_json().await
            } else {
                run_doctor().await
            }
        }
        CliCommand::Unsupported { command } => {
            print_unsupported(&command);
            std::process::exit(1);
        }
        CliCommand::Branch { name, base, labels } => {
            run_branch(&name, base.as_deref(), &labels).await
        }
        CliCommand::List { json } => run_list(json).await,
        CliCommand::Down {
            branch,
            rm,
            volumes,
            force,
        } => run_down(&branch, rm, volumes, force).await,
        CliCommand::Up { branch, rebuild } => run_up(&branch, rebuild).await,
        CliCommand::Exec {
            branch,
            command,
            json,
        } => run_exec(&branch, &command, json).await,
        CliCommand::Prune {
            dry_run,
            all,
            older_than,
            missing_worktree,
            labels,
        } => {
            run_prune(
                dry_run,
                all,
                older_than.as_deref(),
                missing_worktree,
                &labels,
            )
            .await
        }
        CliCommand::TaskRun {
            branch,
            command,
            base,
        } => run_task_run(&branch, &command, base.as_deref()).await,
        CliCommand::TaskList { json } => run_task_list(json).await,
        CliCommand::TaskLogs { branch, follow } => run_task_logs(&branch, follow).await,
        CliCommand::TaskWait { branch } => run_task_wait(&branch).await,
        CliCommand::TaskStop { branch } => run_task_stop(&branch).await,
        CliCommand::Switch { branch } => run_switch(&branch).await,
    }
}

fn print_help() {
    eprintln!(
        "\
cella — dev container worktree management (in-container)

Usage: cella <command> [options]

Commands:
  branch <name> [--base ref]     Create a worktree-backed branch with its own container
  list                           List worktree branches and their containers
  down <branch> [--rm] [--force] Stop a worktree branch's container
  up <branch> [--rebuild]        Start/restart a worktree branch's container
  exec <branch> -- <cmd...>      Run a command in another branch's container
  switch <branch>                Open a shell in another branch's container
  prune [--all] [--dry-run]      Remove worktrees and their containers
  task run <branch> -- <cmd...>  Run a background task in a branch's container
  task list                      List active background tasks
  task logs [-f] <branch>        Show output from a background task (-f to follow)
  task wait <branch>             Wait for a background task to complete
  task stop <branch>             Stop a running background task
  doctor                         Check connectivity and version status

Options:
  --help, -h                     Show this help message

Run `cella --help` on the host for all commands."
    );
}

fn print_command_help(command: &str) {
    let text = match command {
        "branch" => {
            "\
Usage: cella branch <name> [options]

Create a worktree-backed branch with its own container.

Options:
  --base <ref>       Base branch or commit (default: current HEAD)
  --label KEY=VALUE  Add a label to the container (repeatable)"
        }
        "list" | "ls" => {
            "\
Usage: cella list [options]

List worktree branches and their container status.

Options:
  --json    Output as JSON array"
        }
        "exec" => {
            "\
Usage: cella exec <branch> [options] -- <command...>

Run a command in another branch's container.

Options:
  --json    Capture stdout/stderr and output as JSON envelope"
        }
        "down" => {
            "\
Usage: cella down <branch> [options]

Stop a worktree branch's container.

Options:
  --rm        Remove the container and worktree after stopping
  --volumes   Also remove volumes (requires --rm)
  --force     Force stop even when shutdownAction is \"none\""
        }
        "up" => {
            "\
Usage: cella up <branch> [options]

Start or restart a worktree branch's container.

Options:
  --rebuild   Rebuild the container from scratch"
        }
        "prune" => {
            "\
Usage: cella prune [options]

Remove worktrees and their containers.

Options:
  --all               Include unmerged worktrees
  --dry-run           Show what would be pruned without doing it
  --older-than <dur>  Only prune older than duration (e.g., 7d, 24h)
  --missing-worktree  Only prune branches whose worktree is gone
  --label KEY=VALUE   Only prune matching labels (repeatable)"
        }
        "task" => {
            "\
Usage: cella task <subcommand>

Subcommands:
  run <branch> [--base ref] -- <cmd...>   Run a background task
  list [--json]                           List active tasks
  logs [-f|--follow] <branch>             Show task output
  wait <branch>                           Wait for task completion
  stop <branch>                           Stop a running task"
        }
        "doctor" => {
            "\
Usage: cella doctor [options]

Check daemon connectivity and version status.

Options:
  --json    Output structured health data as JSON"
        }
        "switch" => {
            "\
Usage: cella switch <branch>

Open an interactive shell in another branch's container."
        }
        _ => "No help available for this command.",
    };
    eprintln!("{text}");
}

fn print_unsupported(command: &str) {
    eprintln!(
        "\
Error: `cella {command}` is not available inside a dev container.

Available commands inside containers:
  cella branch <name>          Create a worktree-backed branch
  cella list                   List worktree branches
  cella down <branch>          Stop a branch's container
  cella up <branch>            Start/restart a branch's container
  cella exec <branch> -- cmd   Run command in another branch's container
  cella switch <branch>        Shell into another branch's container
  cella prune                  Remove worktrees

Run `cella --help` on the host for all commands."
    );
}

/// Connect to the host daemon for CLI commands.
///
/// Reads connection info from env vars, falling back to the `.daemon_addr`
/// file on the shared agent volume.
async fn connect_daemon() -> Result<ControlClient, Box<dyn std::error::Error + Send + Sync>> {
    let (addr, token) = if let Some(info) = crate::control::read_daemon_addr_file() {
        (info.addr, info.token)
    } else if let Ok(addr) = std::env::var("CELLA_DAEMON_ADDR") {
        let token = std::env::var("CELLA_DAEMON_TOKEN").unwrap_or_default();
        (addr, token)
    } else {
        return Err("No daemon connection info. Are you inside a cella dev container?".into());
    };
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

async fn run_doctor() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let agent_version = env!("CARGO_PKG_VERSION");
    eprintln!("cella doctor (in-container, agent v{agent_version})\n");

    // 1. .daemon_addr file
    let daemon_addr_exists = std::path::Path::new("/cella/.daemon_addr").exists();
    if daemon_addr_exists {
        eprintln!("  \u{2713} daemon address file (/cella/.daemon_addr)");
    } else {
        eprintln!("  \u{2717} daemon address file not found (/cella/.daemon_addr)");
        eprintln!("    Run `cella up` on the host to fix");
    }

    // 2. Resolve connection info (file is authoritative, env vars are fallback)
    let (addr, token) = if let Some(info) = crate::control::read_daemon_addr_file() {
        eprintln!("  \u{2713} connection info (from .daemon_addr file)");
        (info.addr, info.token)
    } else if let Ok(addr) = std::env::var("CELLA_DAEMON_ADDR") {
        let token = std::env::var("CELLA_DAEMON_TOKEN").unwrap_or_default();
        eprintln!("  \u{2713} connection info (from env vars)");
        (addr, token)
    } else {
        eprintln!("  \u{2717} no connection info available");
        eprintln!("    Set CELLA_DAEMON_ADDR or ensure .daemon_addr exists");
        return Ok(());
    };

    // 3. Daemon connectivity
    let container_name = std::env::var("CELLA_CONTAINER_NAME").unwrap_or_default();
    match ControlClient::connect(&addr, &container_name, &token).await {
        Ok((_client, hello)) => {
            eprintln!("  \u{2713} daemon reachable at {addr}");
            eprintln!(
                "  \u{2713} protocol version: {} (matches)",
                cella_protocol::PROTOCOL_VERSION
            );

            // 4. Version comparison
            if hello.daemon_version == agent_version {
                eprintln!("  \u{2713} version: {agent_version}");
            } else {
                eprintln!(
                    "  \u{26a0} agent version {agent_version} != daemon version {}",
                    hello.daemon_version
                );
                eprintln!("    Run `cella up` on the host to update");
            }
        }
        Err(e) => {
            eprintln!("  \u{2717} daemon unreachable at {addr}: {e}");
        }
    }

    // 4b. Main agent process liveness (read state file written by cella-agent daemon)
    report_main_agent_status();

    // 5. Credential helper
    let browser = std::env::var("BROWSER").unwrap_or_default();
    if browser.contains("cella") {
        eprintln!("  \u{2713} browser helper configured");
    } else {
        eprintln!("  \u{26a0} browser helper not configured (BROWSER={browser})");
    }

    eprintln!();
    Ok(())
}

async fn run_doctor_json() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut client = connect_daemon().await?;
    let id = request_id();
    client
        .send(&AgentMessage::DoctorRequest {
            request_id: id.clone(),
        })
        .await?;

    loop {
        let resp = recv_timeout(&mut client, TIMEOUT_FAST).await?;
        if let DaemonMessage::DoctorResult { data, .. } = resp {
            println!("{}", serde_json::to_string_pretty(&data)?);
            return Ok(());
        }
    }
}

fn report_main_agent_status() {
    let snapshot =
        crate::state::read_snapshot(std::path::Path::new(crate::state::DEFAULT_STATE_FILE));
    let lines = format_main_agent_status(
        snapshot.as_ref(),
        crate::state::DEFAULT_STATE_FILE,
        crate::state::pid_alive,
        now_unix_secs(),
    );
    eprint!("{lines}");
}

/// Render the doctor's main-agent status line(s). Pure — no I/O, no globals.
/// Takes a snapshot (or `None` if file missing) and a closure to check PID
/// liveness so tests can inject any state without touching `/proc`.
fn format_main_agent_status(
    snapshot: Option<&crate::state::AgentStateSnapshot>,
    state_file_path: &str,
    pid_alive: impl Fn(u32) -> bool,
    now_unix: u64,
) -> String {
    let Some(snap) = snapshot else {
        return format!(
            "  \u{2717} main agent: state file missing at {state_file_path}\n    Main `cella-agent daemon` process may not be running\n"
        );
    };

    if !pid_alive(snap.pid) {
        return format!(
            "  \u{2717} main agent: process gone (pid {} no longer in /proc)\n    Container needs restart or rebuild\n",
            snap.pid
        );
    }

    let age = now_unix.saturating_sub(snap.last_heartbeat_unix);
    match snap.state {
        crate::state::AgentState::Connected if age < 30 => format!(
            "  \u{2713} main agent: connected (pid {}, heartbeat {age}s ago)\n",
            snap.pid
        ),
        crate::state::AgentState::Connected => format!(
            "  \u{26a0} main agent: connected but heartbeat stale (pid {}, {age}s ago)\n",
            snap.pid
        ),
        crate::state::AgentState::Reconnecting => format!(
            "  \u{26a0} main agent: reconnecting (pid {}, last heartbeat {age}s ago)\n",
            snap.pid
        ),
        crate::state::AgentState::Disconnected => format!(
            "  \u{26a0} main agent: disconnected (pid {}, last heartbeat {age}s ago)\n",
            snap.pid
        ),
    }
}

fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod doctor_tests {
    use super::*;
    use crate::state::{AgentState, AgentStateSnapshot};

    fn snap(state: AgentState, pid: u32, last_heartbeat_unix: u64) -> AgentStateSnapshot {
        AgentStateSnapshot {
            pid,
            state,
            daemon_addr: Some("host.docker.internal:60000".to_string()),
            agent_version: "0.0.28".to_string(),
            started_at_unix: 1_700_000_000,
            last_heartbeat_unix,
        }
    }

    #[test]
    fn status_missing_file() {
        let out = format_main_agent_status(None, "/tmp/cella-agent.state", |_| true, 0);
        assert!(out.contains("\u{2717} main agent: state file missing"));
        assert!(out.contains("/tmp/cella-agent.state"));
    }

    #[test]
    fn status_pid_gone() {
        let s = snap(AgentState::Connected, 12345, 1_700_000_100);
        let out =
            format_main_agent_status(Some(&s), "/tmp/cella-agent.state", |_| false, 1_700_000_100);
        assert!(out.contains("\u{2717} main agent: process gone"));
        assert!(out.contains("pid 12345"));
    }

    #[test]
    fn status_connected_fresh() {
        let now = 1_700_000_100;
        let s = snap(AgentState::Connected, 25, now - 5);
        let out = format_main_agent_status(Some(&s), "/tmp/cella-agent.state", |_| true, now);
        assert!(out.contains("\u{2713} main agent: connected (pid 25"));
        assert!(out.contains("heartbeat 5s ago"));
    }

    #[test]
    fn status_connected_stale() {
        let now = 1_700_000_100;
        // age > 30 triggers the stale branch.
        let s = snap(AgentState::Connected, 25, now - 45);
        let out = format_main_agent_status(Some(&s), "/tmp/cella-agent.state", |_| true, now);
        assert!(out.contains("\u{26a0} main agent: connected but heartbeat stale"));
        assert!(out.contains("45s ago"));
    }

    #[test]
    fn status_reconnecting() {
        let now = 1_700_000_100;
        let s = snap(AgentState::Reconnecting, 25, now - 3);
        let out = format_main_agent_status(Some(&s), "/tmp/cella-agent.state", |_| true, now);
        assert!(out.contains("\u{26a0} main agent: reconnecting (pid 25"));
        assert!(out.contains("3s ago"));
    }

    #[test]
    fn status_disconnected() {
        let now = 1_700_000_100;
        let s = snap(AgentState::Disconnected, 25, now - 1);
        let out = format_main_agent_status(Some(&s), "/tmp/cella-agent.state", |_| true, now);
        assert!(out.contains("\u{26a0} main agent: disconnected (pid 25"));
    }

    #[test]
    fn status_connected_exactly_at_threshold_is_stale() {
        // Boundary: age == 30 is not "< 30", so treated as stale.
        let now = 1_700_000_100;
        let s = snap(AgentState::Connected, 25, now - 30);
        let out = format_main_agent_status(Some(&s), "/tmp/cella-agent.state", |_| true, now);
        assert!(out.contains("connected but heartbeat stale"));
    }
}

async fn run_branch(
    name: &str,
    base: Option<&str>,
    labels: &[String],
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut client = connect_daemon().await?;

    let id = request_id();
    let msg = AgentMessage::BranchRequest {
        request_id: id.clone(),
        branch: name.to_string(),
        base: base.map(String::from),
        labels: if labels.is_empty() {
            None
        } else {
            Some(labels.to_vec())
        },
    };
    client.send(&msg).await?;

    // Read responses until we get the final BranchResult.
    loop {
        let resp = recv_timeout(&mut client, TIMEOUT_SLOW).await?;
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

async fn run_list(json: bool) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut client = connect_daemon().await?;

    let id = request_id();
    let msg = AgentMessage::ListRequest {
        request_id: id.clone(),
    };
    client.send(&msg).await?;

    loop {
        let resp = recv_timeout(&mut client, TIMEOUT_FAST).await?;
        if let DaemonMessage::ListResult { worktrees, .. } = resp {
            if json {
                println!("{}", serde_json::to_string_pretty(&worktrees)?);
            } else if worktrees.is_empty() {
                eprintln!("No worktree branches found.");
            } else {
                print_worktree_table(&worktrees);
            }
            return Ok(());
        }
    }
}

async fn run_exec(
    branch: &str,
    command: &[String],
    json: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut client = connect_daemon().await?;
    let id = request_id();

    if json {
        let msg = AgentMessage::ExecCaptureRequest {
            request_id: id.clone(),
            branch: branch.to_string(),
            command: command.to_vec(),
        };
        client.send(&msg).await?;
        loop {
            let resp = recv_timeout(&mut client, TIMEOUT_MEDIUM).await?;
            if let DaemonMessage::ExecCaptureResult {
                exit_code,
                stdout,
                stderr,
                ..
            } = resp
            {
                let envelope = serde_json::json!({
                    "exit_code": exit_code,
                    "stdout": stdout,
                    "stderr": stderr,
                });
                println!("{}", serde_json::to_string_pretty(&envelope)?);
                if exit_code != 0 {
                    std::process::exit(exit_code);
                }
                return Ok(());
            }
        }
    } else {
        let msg = AgentMessage::ExecRequest {
            request_id: id.clone(),
            branch: branch.to_string(),
            command: command.to_vec(),
        };
        client.send(&msg).await?;
        loop {
            let resp = recv_timeout(&mut client, TIMEOUT_MEDIUM).await?;
            match resp {
                DaemonMessage::OperationOutput { stream, data, .. } => match stream {
                    OutputStream::Stdout => print!("{data}"),
                    OutputStream::Stderr => eprint!("{data}"),
                },
                DaemonMessage::ExecResult { exit_code, .. } => {
                    if exit_code != 0 {
                        std::process::exit(exit_code);
                    }
                    return Ok(());
                }
                _ => {}
            }
        }
    }
}

async fn check_self_target(
    client: &mut ControlClient,
    branch: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let own_container = std::env::var("CELLA_CONTAINER_NAME").unwrap_or_default();
    if own_container.is_empty() {
        return Ok(());
    }

    let msg = AgentMessage::ListRequest {
        request_id: request_id(),
    };
    client.send(&msg).await?;

    loop {
        let resp = recv_timeout(client, TIMEOUT_FAST).await?;
        if let DaemonMessage::ListResult { worktrees, .. } = resp {
            for wt in &worktrees {
                if wt.branch.as_deref() == Some(branch)
                    && wt.container_name.as_deref() == Some(&own_container)
                {
                    return Err("Cannot stop the container you are inside. \
                         Run `cella down` on the host or from another container."
                        .into());
                }
            }
            return Ok(());
        }
    }
}

async fn run_down(
    branch: &str,
    rm: bool,
    volumes: bool,
    force: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut client = connect_daemon().await?;
    check_self_target(&mut client, branch).await?;

    let msg = AgentMessage::DownRequest {
        request_id: request_id(),
        branch: branch.to_string(),
        rm,
        volumes,
        force,
    };
    client.send(&msg).await?;

    loop {
        let resp = recv_timeout(&mut client, TIMEOUT_MEDIUM).await?;
        match resp {
            DaemonMessage::OperationProgress { step, message, .. } => {
                eprintln!("\u{25cf} {step}: {message}");
            }
            DaemonMessage::OperationOutput { stream, data, .. } => match stream {
                OutputStream::Stdout => print!("{data}"),
                OutputStream::Stderr => eprint!("{data}"),
            },
            DaemonMessage::DownResult { result, .. } => match result {
                cella_protocol::DownOperationResult::Success {
                    outcome,
                    container_name,
                } => {
                    let action = match outcome {
                        cella_protocol::DownOutcome::Removed => "Removed",
                        cella_protocol::DownOutcome::Stopped => "Stopped",
                    };
                    if container_name.is_empty() {
                        eprintln!("{action} branch '{branch}'");
                    } else {
                        eprintln!("{action} branch '{branch}' (container: {container_name})");
                    }
                    return Ok(());
                }
                cella_protocol::DownOperationResult::Error { message } => {
                    return Err(format!("Failed to stop branch '{branch}': {message}").into());
                }
            },
            _ => {}
        }
    }
}

async fn run_up(
    branch: &str,
    rebuild: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut client = connect_daemon().await?;

    let id = request_id();
    let msg = AgentMessage::UpRequest {
        request_id: id.clone(),
        branch: branch.to_string(),
        rebuild,
    };
    client.send(&msg).await?;

    loop {
        let resp = recv_timeout(&mut client, TIMEOUT_SLOW).await?;
        match resp {
            DaemonMessage::OperationProgress { step, message, .. } => {
                eprintln!("\u{25cf} {step}: {message}");
            }
            DaemonMessage::OperationOutput { stream, data, .. } => match stream {
                OutputStream::Stdout => print!("{data}"),
                OutputStream::Stderr => eprint!("{data}"),
            },
            DaemonMessage::UpResult { result, .. } => match result {
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
            _ => {}
        }
    }
}

async fn run_prune(
    dry_run: bool,
    all: bool,
    older_than: Option<&str>,
    missing_worktree: bool,
    labels: &[String],
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut client = connect_daemon().await?;

    let id = request_id();
    let msg = AgentMessage::PruneRequest {
        request_id: id.clone(),
        dry_run,
        all,
        older_than: older_than.map(String::from),
        missing_worktree,
        labels: if labels.is_empty() {
            None
        } else {
            Some(labels.to_vec())
        },
    };
    client.send(&msg).await?;

    loop {
        let resp = recv_timeout(&mut client, TIMEOUT_MEDIUM).await?;
        match resp {
            DaemonMessage::OperationOutput { stream, data, .. } => match stream {
                OutputStream::Stdout => print!("{data}"),
                OutputStream::Stderr => eprint!("{data}"),
            },
            DaemonMessage::PruneResult { pruned, errors, .. } => {
                if !pruned.is_empty() {
                    if dry_run {
                        eprintln!("Would prune {} worktree(s).", pruned.len());
                    } else {
                        eprintln!("Pruned {} worktree(s).", pruned.len());
                    }
                }
                if !errors.is_empty() {
                    for e in &errors {
                        eprintln!("Error: {e}");
                    }
                    return Err("prune completed with errors".into());
                }
                return Ok(());
            }
            _ => {}
        }
    }
}

async fn run_task_run(
    branch: &str,
    command: &[String],
    base: Option<&str>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut client = connect_daemon().await?;

    let id = request_id();
    let msg = AgentMessage::TaskRunRequest {
        request_id: id.clone(),
        branch: branch.to_string(),
        command: command.to_vec(),
        base: base.map(String::from),
    };
    client.send(&msg).await?;

    loop {
        let resp = recv_timeout(&mut client, TIMEOUT_SLOW).await?;
        match resp {
            DaemonMessage::OperationProgress { message, .. } => {
                eprintln!("{message}");
            }
            DaemonMessage::OperationOutput { stream, data, .. } => match stream {
                OutputStream::Stdout => print!("{data}"),
                OutputStream::Stderr => eprint!("{data}"),
            },
            DaemonMessage::TaskRunResult { result, .. } => match result {
                cella_protocol::TaskRunOperationResult::Success {
                    task_id,
                    container_name,
                } => {
                    eprintln!("Task '{task_id}' started in container {container_name}");
                    return Ok(());
                }
                cella_protocol::TaskRunOperationResult::Error { message, .. } => {
                    return Err(format!("Failed to start task: {message}").into());
                }
            },
            _ => {}
        }
    }
}

async fn run_task_list(json: bool) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut client = connect_daemon().await?;

    let id = request_id();
    let msg = AgentMessage::TaskListRequest {
        request_id: id.clone(),
    };
    client.send(&msg).await?;

    loop {
        let resp = recv_timeout(&mut client, TIMEOUT_FAST).await?;
        if let DaemonMessage::TaskListResult { tasks, .. } = resp {
            if json {
                println!("{}", serde_json::to_string_pretty(&tasks)?);
            } else if tasks.is_empty() {
                eprintln!("No active tasks.");
            } else {
                const HEADER: &str = "BRANCH               STATUS     TIME     COMMAND";
                println!("{HEADER}");
                for t in &tasks {
                    let status = match t.status {
                        cella_protocol::TaskStatus::Running => "running",
                        cella_protocol::TaskStatus::Done => "done",
                        cella_protocol::TaskStatus::Failed => "failed",
                    };
                    let time = format!("{}s", t.elapsed_secs);
                    let cmd = t.command.join(" ");
                    println!("{:<20} {:<10} {:<8} {cmd}", t.branch, status, time);
                }
            }
            return Ok(());
        }
    }
}

async fn run_task_logs(
    branch: &str,
    follow: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut client = connect_daemon().await?;

    let id = request_id();
    let msg = AgentMessage::TaskLogsRequest {
        request_id: id.clone(),
        branch: branch.to_string(),
        follow,
    };
    client.send(&msg).await?;

    loop {
        let resp = recv_timeout(&mut client, TIMEOUT_MEDIUM).await?;
        if let DaemonMessage::TaskLogsData { data, done, .. } = resp {
            if !data.is_empty() {
                print!("{data}");
            }
            if done {
                return Ok(());
            }
        }
    }
}

async fn run_task_wait(branch: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut client = connect_daemon().await?;

    let id = request_id();
    let msg = AgentMessage::TaskWaitRequest {
        request_id: id.clone(),
        branch: branch.to_string(),
    };
    client.send(&msg).await?;

    // 60s timeout resets on each heartbeat (daemon sends every 30s).
    loop {
        let resp = recv_timeout(&mut client, Duration::from_mins(1)).await?;
        if let DaemonMessage::TaskWaitResult { exit_code, .. } = resp {
            if exit_code != 0 {
                eprintln!("Task '{branch}' exited with code {exit_code}");
                std::process::exit(exit_code);
            }
            eprintln!("Task '{branch}' completed successfully.");
            return Ok(());
        }
        // TaskWaitHeartbeat and other messages just reset the timeout.
    }
}

async fn run_task_stop(branch: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut client = connect_daemon().await?;

    let id = request_id();
    let msg = AgentMessage::TaskStopRequest {
        request_id: id.clone(),
        branch: branch.to_string(),
    };
    client.send(&msg).await?;

    loop {
        let resp = recv_timeout(&mut client, TIMEOUT_FAST).await?;
        match resp {
            DaemonMessage::OperationOutput { data, .. } => eprint!("{data}"),
            DaemonMessage::TaskStopResult { .. } => {
                eprintln!("Task '{branch}' stopped.");
                return Ok(());
            }
            _ => {}
        }
    }
}

async fn run_switch(branch: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut client = connect_daemon().await?;

    let id = request_id();
    let msg = AgentMessage::SwitchRequest {
        request_id: id.clone(),
        branch: branch.to_string(),
    };
    client.send(&msg).await?;

    // Wait for StreamReady or error.
    let stream_port = loop {
        let resp = recv_timeout(&mut client, TIMEOUT_MEDIUM).await?;
        match resp {
            DaemonMessage::StreamReady { port, .. } => break port,
            DaemonMessage::OperationOutput { stream, data, .. } => match stream {
                OutputStream::Stdout => print!("{data}"),
                OutputStream::Stderr => eprint!("{data}"),
            },
            DaemonMessage::SwitchResult { exit_code, .. } => {
                // Got result before stream — error case.
                if exit_code != 0 {
                    std::process::exit(exit_code);
                }
                return Ok(());
            }
            _ => {}
        }
    };

    // Extract daemon host from env var or .daemon_addr file.
    let daemon_addr = std::env::var("CELLA_DAEMON_ADDR")
        .ok()
        .or_else(|| crate::control::read_daemon_addr_file().map(|i| i.addr))
        .unwrap_or_default();
    let host = daemon_addr
        .rsplit_once(':')
        .map_or(daemon_addr.as_str(), |(h, _)| h);
    let stream_addr = format!("{host}:{stream_port}");

    // Connect raw TCP to the stream bridge.
    let stream = tokio::net::TcpStream::connect(&stream_addr).await?;
    let (mut tcp_reader, mut tcp_writer) = stream.into_split();

    // Enter raw terminal mode.
    let saved_termios = enter_raw_mode();

    // Bidirectional forwarding: stdin -> TCP, TCP -> stdout.
    let stdin_task = tokio::spawn(async move {
        let mut stdin = tokio::io::stdin();
        let _ = tokio::io::copy(&mut stdin, &mut tcp_writer).await;
    });

    let stdout_task = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        let _ = tokio::io::copy(&mut tcp_reader, &mut stdout).await;
    });

    // Wait for either direction to end.
    tokio::select! {
        _ = stdin_task => {},
        _ = stdout_task => {},
    }

    // Restore terminal.
    if let Some(termios) = saved_termios {
        restore_terminal(&termios);
    }

    // Wait for SwitchResult on the JSON channel (short timeout — daemon sends it immediately).
    loop {
        let resp = recv_timeout(&mut client, Duration::from_secs(10))
            .await
            .unwrap_or_else(|_| {
                eprintln!("Warning: did not receive exit code from daemon");
                std::process::exit(1);
            });
        if let DaemonMessage::SwitchResult { exit_code, .. } = resp {
            if exit_code != 0 {
                std::process::exit(exit_code);
            }
            return Ok(());
        }
    }
}

/// Enter raw terminal mode, returning the saved termios for later restoration.
fn enter_raw_mode() -> Option<nix::sys::termios::Termios> {
    use nix::sys::termios;

    let stdin = std::io::stdin();
    let original = termios::tcgetattr(&stdin).ok()?;
    let mut raw = original.clone();
    termios::cfmakeraw(&mut raw);
    termios::tcsetattr(&stdin, termios::SetArg::TCSANOW, &raw).ok()?;
    Some(original)
}

/// Restore terminal to the saved termios state.
fn restore_terminal(termios: &nix::sys::termios::Termios) {
    use nix::sys::termios as t;

    let stdin = std::io::stdin();
    let _ = t::tcsetattr(&stdin, t::SetArg::TCSANOW, termios);
}

fn print_worktree_table(worktrees: &[cella_protocol::WorktreeEntry]) {
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
            matches!(cmd, CliCommand::Branch { name, base, .. } if name == "feat/auth" && base.is_none())
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
        assert!(matches!(cmd, CliCommand::Branch { name, base, .. }
            if name == "feat/auth" && base.as_deref() == Some("main")));
    }

    #[test]
    fn parse_list_command() {
        let args = vec!["cella".to_string(), "list".to_string()];
        let cmd = parse_cli_args(&args);
        assert!(matches!(cmd, CliCommand::List { json: false }));
    }

    #[test]
    fn parse_ls_alias() {
        let args = vec!["cella".to_string(), "ls".to_string()];
        let cmd = parse_cli_args(&args);
        assert!(matches!(cmd, CliCommand::List { json: false }));
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
        let args = vec!["cella".to_string(), "status".to_string()];
        let cmd = parse_cli_args(&args);
        assert!(matches!(cmd, CliCommand::Unsupported { command } if command == "status"));
    }

    #[test]
    fn parse_branch_missing_name_shows_help() {
        let args = vec!["cella".to_string(), "branch".to_string()];
        let cmd = parse_cli_args(&args);
        assert!(matches!(cmd, CliCommand::Help));
    }

    #[test]
    fn parse_exec_command() {
        let args = vec![
            "cella".to_string(),
            "exec".to_string(),
            "feat/auth".to_string(),
            "--".to_string(),
            "cargo".to_string(),
            "test".to_string(),
        ];
        let cmd = parse_cli_args(&args);
        assert!(
            matches!(cmd, CliCommand::Exec { branch, command, json: false }
                if branch == "feat/auth" && command == ["cargo", "test"])
        );
    }

    #[test]
    fn parse_exec_missing_separator_shows_help() {
        let args = vec![
            "cella".to_string(),
            "exec".to_string(),
            "feat/auth".to_string(),
        ];
        let cmd = parse_cli_args(&args);
        assert!(matches!(cmd, CliCommand::Help));
    }

    #[test]
    fn parse_prune() {
        let args = vec!["cella".to_string(), "prune".to_string()];
        let cmd = parse_cli_args(&args);
        assert!(matches!(
            cmd,
            CliCommand::Prune {
                dry_run: false,
                all: false,
                ..
            }
        ));
    }

    #[test]
    fn parse_prune_dry_run() {
        let args = vec![
            "cella".to_string(),
            "prune".to_string(),
            "--dry-run".to_string(),
        ];
        let cmd = parse_cli_args(&args);
        assert!(matches!(
            cmd,
            CliCommand::Prune {
                dry_run: true,
                all: false,
                ..
            }
        ));
    }

    #[test]
    fn parse_prune_all() {
        let args = vec![
            "cella".to_string(),
            "prune".to_string(),
            "--all".to_string(),
        ];
        let cmd = parse_cli_args(&args);
        assert!(matches!(
            cmd,
            CliCommand::Prune {
                dry_run: false,
                all: true,
                ..
            }
        ));
    }

    #[test]
    fn parse_down_command() {
        let args = vec![
            "cella".to_string(),
            "down".to_string(),
            "feat/auth".to_string(),
        ];
        let cmd = parse_cli_args(&args);
        assert!(matches!(
            cmd,
            CliCommand::Down {
                branch,
                rm: false,
                volumes: false,
                force: false,
            } if branch == "feat/auth"
        ));
    }

    #[test]
    fn parse_down_with_rm() {
        let args = vec![
            "cella".to_string(),
            "down".to_string(),
            "feat/auth".to_string(),
            "--rm".to_string(),
        ];
        let cmd = parse_cli_args(&args);
        assert!(matches!(
            cmd,
            CliCommand::Down {
                rm: true,
                volumes: false,
                ..
            }
        ));
    }

    #[test]
    fn parse_down_missing_branch_shows_help() {
        let args = vec!["cella".to_string(), "down".to_string()];
        let cmd = parse_cli_args(&args);
        assert!(matches!(cmd, CliCommand::Help));
    }

    #[test]
    fn parse_up_command() {
        let args = vec![
            "cella".to_string(),
            "up".to_string(),
            "feat/auth".to_string(),
        ];
        let cmd = parse_cli_args(&args);
        assert!(matches!(
            cmd,
            CliCommand::Up {
                branch,
                rebuild: false,
            } if branch == "feat/auth"
        ));
    }

    #[test]
    fn parse_up_with_rebuild() {
        let args = vec![
            "cella".to_string(),
            "up".to_string(),
            "feat/auth".to_string(),
            "--rebuild".to_string(),
        ];
        let cmd = parse_cli_args(&args);
        assert!(matches!(cmd, CliCommand::Up { rebuild: true, .. }));
    }

    #[test]
    fn parse_up_missing_branch_shows_help() {
        let args = vec!["cella".to_string(), "up".to_string()];
        let cmd = parse_cli_args(&args);
        assert!(matches!(cmd, CliCommand::Help));
    }

    // --- task subcommand tests ---

    #[test]
    fn parse_task_run() {
        let args: Vec<String> = ["cella", "task", "run", "feat/ci", "--", "cargo", "test"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let cmd = parse_cli_args(&args);
        assert!(matches!(cmd, CliCommand::TaskRun { branch, command, base }
                if branch == "feat/ci"
                    && command == ["cargo", "test"]
                    && base.is_none()));
    }

    #[test]
    fn parse_task_run_with_base() {
        let args: Vec<String> = [
            "cella", "task", "run", "feat/ci", "--base", "develop", "--", "make", "build",
        ]
        .iter()
        .map(ToString::to_string)
        .collect();
        let cmd = parse_cli_args(&args);
        assert!(matches!(cmd, CliCommand::TaskRun { branch, command, base }
                if branch == "feat/ci"
                    && command == ["make", "build"]
                    && base.as_deref() == Some("develop")));
    }

    #[test]
    fn parse_task_run_missing_command_shows_help() {
        let args: Vec<String> = ["cella", "task", "run", "feat/ci"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let cmd = parse_cli_args(&args);
        assert!(matches!(cmd, CliCommand::Help));
    }

    #[test]
    fn parse_task_run_missing_branch_shows_help() {
        let args: Vec<String> = ["cella", "task", "run"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let cmd = parse_cli_args(&args);
        assert!(matches!(cmd, CliCommand::Help));
    }

    #[test]
    fn parse_task_list() {
        let args: Vec<String> = ["cella", "task", "list"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let cmd = parse_cli_args(&args);
        assert!(matches!(cmd, CliCommand::TaskList { json: false }));
    }

    #[test]
    fn parse_task_ls_alias() {
        let args: Vec<String> = ["cella", "task", "ls"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let cmd = parse_cli_args(&args);
        assert!(matches!(cmd, CliCommand::TaskList { json: false }));
    }

    #[test]
    fn parse_task_list_json() {
        let args: Vec<String> = ["cella", "task", "list", "--json"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let cmd = parse_cli_args(&args);
        assert!(matches!(cmd, CliCommand::TaskList { json: true }));
    }

    #[test]
    fn parse_task_logs() {
        let args: Vec<String> = ["cella", "task", "logs", "feat/ci"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let cmd = parse_cli_args(&args);
        assert!(matches!(cmd, CliCommand::TaskLogs { branch, follow }
                if branch == "feat/ci" && !follow));
    }

    #[test]
    fn parse_task_logs_follow() {
        let args: Vec<String> = ["cella", "task", "logs", "-f", "feat/ci"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let cmd = parse_cli_args(&args);
        assert!(matches!(cmd, CliCommand::TaskLogs { branch, follow }
                if branch == "feat/ci" && follow));
    }

    #[test]
    fn parse_task_logs_follow_long() {
        let args: Vec<String> = ["cella", "task", "logs", "--follow", "feat/ci"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let cmd = parse_cli_args(&args);
        assert!(matches!(cmd, CliCommand::TaskLogs { branch, follow }
                if branch == "feat/ci" && follow));
    }

    #[test]
    fn parse_task_logs_missing_branch_shows_help() {
        let args: Vec<String> = ["cella", "task", "logs"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let cmd = parse_cli_args(&args);
        assert!(matches!(cmd, CliCommand::Help));
    }

    #[test]
    fn parse_task_wait() {
        let args: Vec<String> = ["cella", "task", "wait", "feat/ci"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let cmd = parse_cli_args(&args);
        assert!(matches!(cmd, CliCommand::TaskWait { branch } if branch == "feat/ci"));
    }

    #[test]
    fn parse_task_wait_missing_branch_shows_help() {
        let args: Vec<String> = ["cella", "task", "wait"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let cmd = parse_cli_args(&args);
        assert!(matches!(cmd, CliCommand::Help));
    }

    #[test]
    fn parse_task_stop() {
        let args: Vec<String> = ["cella", "task", "stop", "feat/ci"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let cmd = parse_cli_args(&args);
        assert!(matches!(cmd, CliCommand::TaskStop { branch } if branch == "feat/ci"));
    }

    #[test]
    fn parse_task_stop_missing_branch_shows_help() {
        let args: Vec<String> = ["cella", "task", "stop"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let cmd = parse_cli_args(&args);
        assert!(matches!(cmd, CliCommand::Help));
    }

    #[test]
    fn parse_task_unknown_subcommand_shows_help() {
        let args: Vec<String> = ["cella", "task", "bogus"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let cmd = parse_cli_args(&args);
        assert!(matches!(cmd, CliCommand::Help));
    }

    #[test]
    fn parse_task_no_subcommand_shows_help() {
        let args: Vec<String> = ["cella", "task"].iter().map(ToString::to_string).collect();
        let cmd = parse_cli_args(&args);
        assert!(matches!(cmd, CliCommand::Help));
    }

    // --- switch/down flag tests ---

    #[test]
    fn parse_switch_command() {
        let args: Vec<String> = ["cella", "switch", "feat/other"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let cmd = parse_cli_args(&args);
        assert!(matches!(cmd, CliCommand::Switch { branch } if branch == "feat/other"));
    }

    #[test]
    fn parse_switch_missing_branch_shows_help() {
        let args: Vec<String> = ["cella", "switch"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let cmd = parse_cli_args(&args);
        assert!(matches!(cmd, CliCommand::Help));
    }

    #[test]
    fn parse_down_all_flags() {
        let args: Vec<String> = ["cella", "down", "feat/auth", "--rm", "--volumes", "--force"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let cmd = parse_cli_args(&args);
        assert!(matches!(
            cmd,
            CliCommand::Down {
                branch,
                rm: true,
                volumes: true,
                force: true,
            } if branch == "feat/auth"
        ));
    }

    #[test]
    fn parse_down_volumes_without_rm_rejected() {
        let args: Vec<String> = ["cella", "down", "feat/auth", "--volumes"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let cmd = parse_cli_args(&args);
        assert!(matches!(cmd, CliCommand::Help));
    }

    #[test]
    fn parse_down_force_only() {
        let args: Vec<String> = ["cella", "down", "feat/auth", "--force"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let cmd = parse_cli_args(&args);
        assert!(matches!(
            cmd,
            CliCommand::Down {
                force: true,
                rm: false,
                volumes: false,
                ..
            }
        ));
    }

    // --- request_id tests ---

    #[test]
    fn request_id_increments() {
        let id1 = request_id();
        let id2 = request_id();
        let id3 = request_id();

        assert!(id1.starts_with("cli-"));
        assert!(id2.starts_with("cli-"));
        assert!(id3.starts_with("cli-"));

        // Parse the numeric parts and verify they increment.
        let n1: u64 = id1.strip_prefix("cli-").unwrap().parse().unwrap();
        let n2: u64 = id2.strip_prefix("cli-").unwrap().parse().unwrap();
        let n3: u64 = id3.strip_prefix("cli-").unwrap().parse().unwrap();
        assert!(n2 > n1);
        assert!(n3 > n2);
    }

    // --- print functions don't panic ---

    #[test]
    fn print_help_does_not_panic() {
        // Just verifying it does not panic; output goes to stderr.
        print_help();
    }

    #[test]
    fn print_unsupported_does_not_panic() {
        print_unsupported("bogus");
    }

    #[test]
    fn print_worktree_table_does_not_panic() {
        let worktrees = vec![
            cella_protocol::WorktreeEntry {
                branch: Some("main".to_string()),
                worktree_path: "/home/user/project".to_string(),
                is_main: true,
                container_name: Some("project-main".to_string()),
                container_state: Some("running".to_string()),
                container_id: None,
                labels: None,
            },
            cella_protocol::WorktreeEntry {
                branch: Some("feat/auth".to_string()),
                worktree_path: "/home/user/project-auth".to_string(),
                is_main: false,
                container_name: None,
                container_state: None,
                container_id: None,
                labels: None,
            },
            cella_protocol::WorktreeEntry {
                branch: None,
                worktree_path: "/home/user/project-detached".to_string(),
                is_main: false,
                container_name: Some("detached-ctr".to_string()),
                container_state: Some("exited".to_string()),
                container_id: None,
                labels: None,
            },
        ];
        print_worktree_table(&worktrees);
    }

    #[test]
    fn parse_h_flag() {
        let args: Vec<String> = ["cella", "-h"].iter().map(ToString::to_string).collect();
        let cmd = parse_cli_args(&args);
        assert!(matches!(cmd, CliCommand::Help));
    }

    #[test]
    fn parse_branch_name_starting_with_dash_shows_help() {
        let args: Vec<String> = ["cella", "branch", "--bad"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let cmd = parse_cli_args(&args);
        assert!(matches!(cmd, CliCommand::Help));
    }

    #[test]
    fn parse_exec_branch_starting_with_dash_shows_help() {
        let args: Vec<String> = ["cella", "exec", "--bad", "--", "ls"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let cmd = parse_cli_args(&args);
        assert!(matches!(cmd, CliCommand::Help));
    }

    #[test]
    fn parse_prune_all_and_dry_run() {
        let args: Vec<String> = ["cella", "prune", "--all", "--dry-run"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let cmd = parse_cli_args(&args);
        assert!(matches!(
            cmd,
            CliCommand::Prune {
                dry_run: true,
                all: true,
                ..
            }
        ));
    }

    #[test]
    fn parse_up_rebuild() {
        let args: Vec<String> = ["cella", "up", "my-branch", "--rebuild"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let cmd = parse_cli_args(&args);
        assert!(matches!(cmd, CliCommand::Up { branch, rebuild: true } if branch == "my-branch"));
    }

    #[test]
    fn parse_doctor_command() {
        let args: Vec<String> = ["cella", "doctor"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let cmd = parse_cli_args(&args);
        assert!(matches!(cmd, CliCommand::Doctor { json: false }));
    }

    #[test]
    fn parse_branch_base_missing_value_shows_help() {
        let args: Vec<String> = ["cella", "branch", "my-branch", "--base"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let cmd = parse_cli_args(&args);
        assert!(matches!(cmd, CliCommand::Help));
    }

    #[test]
    fn parse_branch_base_value_starts_with_dash_shows_help() {
        let args: Vec<String> = ["cella", "branch", "my-branch", "--base", "--other"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let cmd = parse_cli_args(&args);
        assert!(matches!(cmd, CliCommand::Help));
    }

    #[test]
    fn parse_branch_rejects_unknown_flags() {
        let args: Vec<String> = ["cella", "branch", "my-branch", "--unknown"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let cmd = parse_cli_args(&args);
        assert!(matches!(cmd, CliCommand::Help));
    }

    #[test]
    fn parse_exec_branch_is_separator_shows_help() {
        let args: Vec<String> = ["cella", "exec", "--", "ls"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let cmd = parse_cli_args(&args);
        assert!(matches!(cmd, CliCommand::Help));
    }

    #[test]
    fn parse_exec_empty_command_after_separator_shows_help() {
        let args: Vec<String> = ["cella", "exec", "feat/x", "--"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let cmd = parse_cli_args(&args);
        assert!(matches!(cmd, CliCommand::Help));
    }

    #[test]
    fn parse_exec_no_branch_shows_help() {
        let args: Vec<String> = ["cella", "exec"].iter().map(ToString::to_string).collect();
        let cmd = parse_cli_args(&args);
        assert!(matches!(cmd, CliCommand::Help));
    }

    #[test]
    fn parse_down_branch_starts_with_dash_shows_help() {
        let args: Vec<String> = ["cella", "down", "--bad"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let cmd = parse_cli_args(&args);
        assert!(matches!(cmd, CliCommand::Help));
    }

    #[test]
    fn parse_up_branch_starts_with_dash_shows_help() {
        let args: Vec<String> = ["cella", "up", "--bad"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let cmd = parse_cli_args(&args);
        assert!(matches!(cmd, CliCommand::Help));
    }

    #[test]
    fn parse_switch_branch_starts_with_dash_shows_help() {
        let args: Vec<String> = ["cella", "switch", "--bad"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let cmd = parse_cli_args(&args);
        assert!(matches!(cmd, CliCommand::Help));
    }

    #[test]
    fn parse_task_run_branch_starts_with_dash_shows_help() {
        let args: Vec<String> = ["cella", "task", "run", "--bad", "--", "ls"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let cmd = parse_cli_args(&args);
        assert!(matches!(cmd, CliCommand::Help));
    }

    #[test]
    fn parse_task_run_branch_is_separator_shows_help() {
        let args: Vec<String> = ["cella", "task", "run", "--", "ls"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let cmd = parse_cli_args(&args);
        assert!(matches!(cmd, CliCommand::Help));
    }

    #[test]
    fn parse_task_run_base_missing_value_shows_help() {
        let args: Vec<String> = ["cella", "task", "run", "feat/x", "--base", "--", "ls"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let cmd = parse_cli_args(&args);
        assert!(matches!(cmd, CliCommand::Help));
    }

    #[test]
    fn parse_task_wait_branch_starts_with_dash_shows_help() {
        let args: Vec<String> = ["cella", "task", "wait", "--bad"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let cmd = parse_cli_args(&args);
        assert!(matches!(cmd, CliCommand::Help));
    }

    #[test]
    fn parse_task_stop_branch_starts_with_dash_shows_help() {
        let args: Vec<String> = ["cella", "task", "stop", "--bad"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let cmd = parse_cli_args(&args);
        assert!(matches!(cmd, CliCommand::Help));
    }

    #[test]
    fn parse_task_logs_only_flags_shows_help() {
        // Only -f, no branch name
        let args: Vec<String> = ["cella", "task", "logs", "-f"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let cmd = parse_cli_args(&args);
        assert!(matches!(cmd, CliCommand::Help));
    }

    #[test]
    fn timeout_constants_ordering() {
        assert!(TIMEOUT_FAST < TIMEOUT_MEDIUM);
        assert!(TIMEOUT_MEDIUM < TIMEOUT_SLOW);
    }

    #[test]
    fn timeout_fast_is_30s() {
        assert_eq!(TIMEOUT_FAST, Duration::from_secs(30));
    }

    #[test]
    fn timeout_medium_is_120s() {
        assert_eq!(TIMEOUT_MEDIUM, Duration::from_mins(2));
    }

    #[test]
    fn timeout_slow_is_600s() {
        assert_eq!(TIMEOUT_SLOW, Duration::from_mins(10));
    }

    #[test]
    fn print_worktree_table_empty() {
        // Should not panic on empty slice.
        print_worktree_table(&[]);
    }

    // --- JSON flag parsing ---

    #[test]
    fn parse_list_json() {
        let args: Vec<String> = ["cella", "list", "--json"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let cmd = parse_cli_args(&args);
        assert!(matches!(cmd, CliCommand::List { json: true }));
    }

    #[test]
    fn parse_exec_json() {
        let args: Vec<String> = ["cella", "exec", "feat/x", "--json", "--", "echo", "hi"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let cmd = parse_cli_args(&args);
        assert!(matches!(cmd, CliCommand::Exec { branch, json: true, .. } if branch == "feat/x"));
    }

    #[test]
    fn parse_doctor_json() {
        let args: Vec<String> = ["cella", "doctor", "--json"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let cmd = parse_cli_args(&args);
        assert!(matches!(cmd, CliCommand::Doctor { json: true }));
    }

    // --- per-command help ---

    #[test]
    fn parse_branch_help() {
        let args: Vec<String> = ["cella", "branch", "--help"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let cmd = parse_cli_args(&args);
        assert!(matches!(cmd, CliCommand::CommandHelp));
    }

    #[test]
    fn parse_task_help() {
        let args: Vec<String> = ["cella", "task", "--help"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let cmd = parse_cli_args(&args);
        assert!(matches!(cmd, CliCommand::CommandHelp));
    }

    // --- branch name validation ---

    #[test]
    fn parse_branch_whitespace_rejected() {
        let args: Vec<String> = ["cella", "branch", "bad name"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let cmd = parse_cli_args(&args);
        assert!(matches!(cmd, CliCommand::Help));
    }

    #[test]
    fn print_command_help_does_not_panic() {
        for cmd in &[
            "branch", "list", "exec", "down", "up", "prune", "task", "doctor", "switch", "bogus",
        ] {
            print_command_help(cmd);
        }
    }
}
