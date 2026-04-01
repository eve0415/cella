//! In-container CLI mode for cella.
//!
//! When the agent binary is invoked as `cella` (via symlink at `/cella/bin/cella`),
//! it enters CLI mode instead of daemon mode. CLI commands delegate to the host
//! daemon via the existing TCP control connection.

use std::time::Duration;

use cella_port::protocol::{AgentMessage, DaemonMessage, OutputStream, WorktreeOperationResult};
use tokio::time::timeout;

use crate::control::ControlClient;

/// Timeout for fast operations (list, task list, task stop).
const TIMEOUT_FAST: Duration = Duration::from_secs(30);
/// Timeout for medium operations (down, exec, prune, task logs/wait).
const TIMEOUT_MEDIUM: Duration = Duration::from_secs(120);
/// Timeout for slow operations (branch, up, task run).
const TIMEOUT_SLOW: Duration = Duration::from_secs(600);

/// Receive a message with a timeout that resets on each message in the loop.
async fn recv_timeout(
    client: &mut ControlClient,
    dur: Duration,
) -> Result<DaemonMessage, Box<dyn std::error::Error>> {
    Ok(timeout(dur, client.recv())
        .await
        .map_err(|_| -> Box<dyn std::error::Error> {
            "timed out waiting for response from daemon".into()
        })??)
}

/// In-container CLI commands.
pub enum CliCommand {
    Branch {
        name: String,
        base: Option<String>,
    },
    List,
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
    },
    Prune {
        dry_run: bool,
        all: bool,
    },
    TaskRun {
        branch: String,
        command: Vec<String>,
        base: Option<String>,
    },
    TaskList,
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
    Doctor,
    Help,
    Unsupported {
        command: String,
    },
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
                } else {
                    i += 1;
                }
            }
            CliCommand::Branch { name, base }
        }
        Some("list" | "ls") => CliCommand::List,
        Some("exec") => {
            // Parse: cella exec <branch> -- <cmd...>
            let branch = match args.get(2) {
                Some(b) if !b.starts_with('-') && b != "--" => b.clone(),
                _ => return CliCommand::Help,
            };
            // Find "--" separator
            let sep = args.iter().position(|a| a == "--");
            let command = sep.map_or_else(Vec::new, |i| args[i + 1..].to_vec());
            if command.is_empty() {
                return CliCommand::Help;
            }
            CliCommand::Exec { branch, command }
        }
        Some("down") => {
            let branch = match args.get(2) {
                Some(b) if !b.starts_with('-') => b.clone(),
                _ => return CliCommand::Help,
            };
            let rm = args.iter().any(|a| a == "--rm");
            let volumes = args.iter().any(|a| a == "--volumes");
            let force = args.iter().any(|a| a == "--force");
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
            let rebuild = args.iter().any(|a| a == "--rebuild");
            CliCommand::Up { branch, rebuild }
        }
        Some("prune") => {
            let dry_run = args.iter().any(|a| a == "--dry-run");
            let all = args.iter().any(|a| a == "--all");
            CliCommand::Prune { dry_run, all }
        }
        Some("task") => parse_task_subcommand(args),
        Some("switch") => {
            let branch = match args.get(2) {
                Some(b) if !b.starts_with('-') => b.clone(),
                _ => return CliCommand::Help,
            };
            CliCommand::Switch { branch }
        }
        Some("doctor") => CliCommand::Doctor,
        Some("--help" | "-h") | None => CliCommand::Help,
        Some(cmd) => CliCommand::Unsupported {
            command: cmd.to_string(),
        },
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
        Some("list" | "ls") => CliCommand::TaskList,
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
pub async fn run(command: CliCommand) -> Result<(), Box<dyn std::error::Error>> {
    match command {
        CliCommand::Help => {
            print_help();
            Ok(())
        }
        CliCommand::Doctor => run_doctor().await,
        CliCommand::Unsupported { command } => {
            print_unsupported(&command);
            std::process::exit(1);
        }
        CliCommand::Branch { name, base } => run_branch(&name, base.as_deref()).await,
        CliCommand::List => run_list().await,
        CliCommand::Down {
            branch,
            rm,
            volumes,
            force,
        } => run_down(&branch, rm, volumes, force).await,
        CliCommand::Up { branch, rebuild } => run_up(&branch, rebuild).await,
        CliCommand::Exec { branch, command } => run_exec(&branch, &command).await,
        CliCommand::Prune { dry_run, all } => run_prune(dry_run, all).await,
        CliCommand::TaskRun {
            branch,
            command,
            base,
        } => run_task_run(&branch, &command, base.as_deref()).await,
        CliCommand::TaskList => run_task_list().await,
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
async fn connect_daemon() -> Result<ControlClient, Box<dyn std::error::Error>> {
    let (addr, token) = if let Ok(addr) = std::env::var("CELLA_DAEMON_ADDR") {
        let token = std::env::var("CELLA_DAEMON_TOKEN").unwrap_or_default();
        (addr, token)
    } else if let Some(info) = crate::control::read_daemon_addr_file() {
        (info.addr, info.token)
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

async fn run_doctor() -> Result<(), Box<dyn std::error::Error>> {
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

    // 2. Resolve connection info
    let (addr, token) = if let Ok(addr) = std::env::var("CELLA_DAEMON_ADDR") {
        let token = std::env::var("CELLA_DAEMON_TOKEN").unwrap_or_default();
        eprintln!("  \u{2713} connection info (from env vars)");
        (addr, token)
    } else if let Some(info) = crate::control::read_daemon_addr_file() {
        eprintln!("  \u{2713} connection info (from .daemon_addr file)");
        (info.addr, info.token)
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
                cella_port::protocol::PROTOCOL_VERSION
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

async fn run_list() -> Result<(), Box<dyn std::error::Error>> {
    let mut client = connect_daemon().await?;

    let id = request_id();
    let msg = AgentMessage::ListRequest {
        request_id: id.clone(),
    };
    client.send(&msg).await?;

    loop {
        let resp = recv_timeout(&mut client, TIMEOUT_FAST).await?;
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

async fn run_exec(branch: &str, command: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let mut client = connect_daemon().await?;

    let id = request_id();
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

async fn check_self_target(
    client: &mut ControlClient,
    branch: &str,
) -> Result<(), Box<dyn std::error::Error>> {
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
) -> Result<(), Box<dyn std::error::Error>> {
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
                cella_port::protocol::DownOperationResult::Success {
                    outcome,
                    container_name,
                } => {
                    let action = match outcome {
                        cella_port::protocol::DownOutcome::Removed => "Removed",
                        cella_port::protocol::DownOutcome::Stopped => "Stopped",
                    };
                    if container_name.is_empty() {
                        eprintln!("{action} branch '{branch}'");
                    } else {
                        eprintln!("{action} branch '{branch}' (container: {container_name})");
                    }
                    return Ok(());
                }
                cella_port::protocol::DownOperationResult::Error { message } => {
                    return Err(format!("Failed to stop branch '{branch}': {message}").into());
                }
            },
            _ => {}
        }
    }
}

async fn run_up(branch: &str, rebuild: bool) -> Result<(), Box<dyn std::error::Error>> {
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

async fn run_prune(dry_run: bool, all: bool) -> Result<(), Box<dyn std::error::Error>> {
    let mut client = connect_daemon().await?;

    let id = request_id();
    let msg = AgentMessage::PruneRequest {
        request_id: id.clone(),
        dry_run,
        all,
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
                    eprintln!("Pruned {} worktree(s).", pruned.len());
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
) -> Result<(), Box<dyn std::error::Error>> {
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
                cella_port::protocol::TaskRunOperationResult::Success {
                    task_id,
                    container_name,
                } => {
                    eprintln!("Task '{task_id}' started in container {container_name}");
                    return Ok(());
                }
                cella_port::protocol::TaskRunOperationResult::Error { message } => {
                    return Err(format!("Failed to start task: {message}").into());
                }
            },
            _ => {}
        }
    }
}

async fn run_task_list() -> Result<(), Box<dyn std::error::Error>> {
    let mut client = connect_daemon().await?;

    let id = request_id();
    let msg = AgentMessage::TaskListRequest {
        request_id: id.clone(),
    };
    client.send(&msg).await?;

    loop {
        let resp = recv_timeout(&mut client, TIMEOUT_FAST).await?;
        if let DaemonMessage::TaskListResult { tasks, .. } = resp {
            if tasks.is_empty() {
                eprintln!("No active tasks.");
            } else {
                const HEADER: &str = "BRANCH               STATUS     TIME     COMMAND";
                println!("{HEADER}");
                for t in &tasks {
                    let status = match t.status {
                        cella_port::protocol::TaskStatus::Running => "running",
                        cella_port::protocol::TaskStatus::Done => "done",
                        cella_port::protocol::TaskStatus::Failed => "failed",
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

async fn run_task_logs(branch: &str, follow: bool) -> Result<(), Box<dyn std::error::Error>> {
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

async fn run_task_wait(branch: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut client = connect_daemon().await?;

    let id = request_id();
    let msg = AgentMessage::TaskWaitRequest {
        request_id: id.clone(),
        branch: branch.to_string(),
    };
    client.send(&msg).await?;

    loop {
        let resp = recv_timeout(&mut client, TIMEOUT_MEDIUM).await?;
        if let DaemonMessage::TaskWaitResult { exit_code, .. } = resp {
            if exit_code != 0 {
                eprintln!("Task '{branch}' exited with code {exit_code}");
                std::process::exit(exit_code);
            }
            eprintln!("Task '{branch}' completed successfully.");
            return Ok(());
        }
    }
}

async fn run_task_stop(branch: &str) -> Result<(), Box<dyn std::error::Error>> {
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

async fn run_switch(branch: &str) -> Result<(), Box<dyn std::error::Error>> {
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
        assert!(matches!(cmd, CliCommand::Exec { branch, command }
                if branch == "feat/auth" && command == ["cargo", "test"]));
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
                all: false
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
                all: false
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
                all: true
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
}
