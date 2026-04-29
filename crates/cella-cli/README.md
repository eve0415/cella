# cella-cli

> CLI entry point for cella. Command routing, argument parsing, and user-facing output.

Part of the [cella](../../README.md) workspace.

## Overview

cella-cli is the binary frontend for cella. It defines the `cella` binary, parses CLI arguments via clap, and dispatches each subcommand to the appropriate library crate. No business logic lives here — every command handler delegates to library crates for the actual work.

Error reporting uses miette's graphical handler for source-positioned diagnostics. Tracing is configured via the `RUST_LOG` environment variable. Terminal window titles are updated via OSC escape sequences for the duration of long-running commands and restored on exit.

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
| `code` | Open VS Code connected to the dev container |
| `nvim` | Open Neovim connected to the dev container |
| `tmux` | Open a tmux session inside the dev container |
| `ports` | View port forwarding status for dev containers |
| `credential` | Manage credential forwarding for dev containers |
| `network` | Manage network proxy settings for dev containers |
| `features` | Manage devcontainer features (edit, list, update) |
| `completions` | Generate shell completions |
| `read-configuration` | Read and output the resolved devcontainer configuration (JSON) |

Hidden internal commands: `daemon`.

### Global Flags

| Flag | Description |
|------|-------------|
| `--backend <name>` | Container backend to use (`docker` or `apple-container`). Defaults to auto-detection. |

## Architecture

### Key Types

- `Cli` — top-level clap parser (`#[derive(Parser)]`)
- `Command` — enum of all subcommands, each variant holds its own args struct

### Modules

| Module | Purpose |
|--------|---------|
| `commands/mod.rs` | `Command` enum and dispatch logic |
| `commands/up/` | Container startup orchestration (largest command — handles single-container and compose paths) |
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
| `commands/init/` | Interactive project initialization wizard (mod.rs, wizard.rs, noninteractive.rs, summary.rs) |
| `commands/code.rs` | VS Code container connection (attached-container URI, auto-up, fork support) |
| `commands/nvim.rs` | Neovim container connection |
| `commands/tmux.rs` | tmux session management (attach-or-create, config forwarding) |
| `commands/ports.rs` | Port forwarding status |
| `commands/credential.rs` | Credential forwarding management |
| `commands/daemon.rs` | Daemon lifecycle management (hidden) |
| `commands/network.rs` | Network proxy management |
| `commands/completions.rs` | Shell completion generation |
| `commands/features/` | Feature management (mod.rs, edit.rs, jsonc_edit.rs, list.rs, prompts.rs, resolve.rs, update.rs) |
| `backend.rs` | Backend selection and auto-detection (`--backend` flag resolution) |
| `picker.rs` | Interactive selection picker |
| `progress.rs` | Progress reporting (indicatif spinners, verbosity) |
| `style.rs` | Output styling and colors |
| `table.rs` | Table formatting for list output |
| `title.rs` | Terminal title integration (sets/restores window title via OSC escape sequences, tmux-aware) |

Each command file defines an args struct and an `execute()` method.

## Crate Dependencies

**Depends on:** [cella-backend](../cella-backend), [cella-compose](../cella-compose), [cella-config](../cella-config), [cella-container](../cella-container) (macOS only, gated on `cfg(target_os = "macos")`), [cella-daemon](../cella-daemon), [cella-doctor](../cella-doctor), [cella-docker](../cella-docker), [cella-env](../cella-env), [cella-git](../cella-git), [cella-jsonc](../cella-jsonc), [cella-network](../cella-network), [cella-orchestrator](../cella-orchestrator), [cella-protocol](../cella-protocol), [cella-templates](../cella-templates)

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
