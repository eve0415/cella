# cella-cli

> CLI entry point for cella. Command routing, argument parsing, and user-facing output.

Part of the [cella](../../README.md) workspace.

## Overview

cella-cli is the binary frontend for cella. It defines the `cella` binary, parses CLI arguments via clap, and dispatches each subcommand to the appropriate library crate. No business logic lives here — every command handler delegates to library crates for the actual work.

Error reporting uses miette's graphical handler for source-positioned diagnostics. Tracing is configured via the `RUST_LOG` environment variable.

## Commands

| Command | Description |
|---------|-------------|
| `up` | Start a dev container for the current workspace |
| `down` | Stop and remove the dev container |
| `shell` | Open a shell inside the running dev container |
| `exec` | Execute a command inside the running dev container |
| `build` | Build the dev container image without starting it |
| `list` | List all dev containers managed by cella |
| `logs` | View logs from the dev container |
| `doctor` | Check system dependencies and configuration |
| `branch` | Create a new worktree-backed branch with its own dev container |
| `switch` | Switch to a different worktree-backed branch |
| `prune` | Remove stale worktrees and their associated containers |
| `config` | View and manage cella configuration |
| `template` | Manage dev container templates |
| `init` | Initialize cella in the current repository |
| `nvim` | Open neovim connected to the dev container |
| `ports` | View port forwarding status for dev containers |
| `credential` | Manage credential forwarding for dev containers |
| `read-configuration` | Read and output the resolved devcontainer configuration (JSON) |

Hidden internal commands: `daemon`, `credential-proxy`.

## Architecture

### Key Types

- `Cli` — top-level clap parser (`#[derive(Parser)]`)
- `Command` — enum of all subcommands, each variant holds its own args struct

### Modules

| Module | Purpose |
|--------|---------|
| `commands/mod.rs` | `Command` enum and dispatch logic |
| `commands/up.rs` | Container startup orchestration (largest command — handles single-container and compose paths) |
| `commands/compose_up.rs` | Docker Compose orchestration (called by `up` when `dockerComposeFile` is present) |
| `commands/down.rs` | Container teardown |
| `commands/shell.rs` | Interactive shell attach |
| `commands/exec.rs` | Command execution inside containers |
| `commands/build.rs` | Image building |
| `commands/list.rs` | Container listing with status and ports |
| `commands/logs.rs` | Container log streaming (`--follow`, `--lifecycle`, `--daemon`) |
| `commands/doctor.rs` | System diagnostics orchestration |
| `commands/branch.rs` | Worktree-backed branch creation |
| `commands/switch.rs` | Worktree branch switching |
| `commands/prune.rs` | Stale worktree and container cleanup |
| `commands/config.rs` | Configuration management |
| `commands/read_configuration.rs` | Resolved devcontainer config output (JSON, compatible with devcontainer CLI) |
| `commands/template.rs` | Template management |
| `commands/init.rs` | Repository initialization |
| `commands/nvim.rs` | Neovim container connection |
| `commands/ports.rs` | Port forwarding status |
| `commands/credential.rs` | Credential forwarding management |
| `commands/daemon.rs` | Daemon lifecycle management (hidden) |
| `commands/credential_proxy.rs` | Legacy credential proxy management (hidden) |
| `commands/env_cache.rs` | Environment cache management (internal helper) |
| `commands/image.rs` | Image inspection (internal helper) |

Each command file defines an args struct and an `execute()` method.

## Crate Dependencies

**Depends on:** [cella-config](../cella-config), [cella-docker](../cella-docker), [cella-compose](../cella-compose), [cella-env](../cella-env), [cella-features](../cella-features), [cella-git](../cella-git), [cella-port](../cella-port), [cella-agent](../cella-agent), [cella-daemon](../cella-daemon), [cella-doctor](../cella-doctor), [cella-credential-proxy](../cella-credential-proxy)

**Depended on by:** none (top of the dependency tree)

## Testing

```sh
cargo test -p cella-cli
```

Tests cover argument parsing and command dispatch. End-to-end integration tests require a running Docker daemon and are marked with `#[ignore]`.

## Development

If you're implementing a new feature, the logic belongs in a library crate — this crate should only contain the clap args struct and a thin `execute()` method that calls into library code. The `up` command is the exception due to its orchestration complexity, but even it delegates container operations to cella-docker.

To add a new command:
1. Create `commands/<name>.rs` with an args struct and `execute()` method
2. Add the variant to the `Command` enum in `commands/mod.rs`
3. Add the dispatch arm in `Command::execute()`
