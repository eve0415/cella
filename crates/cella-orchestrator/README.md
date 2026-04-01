# cella-orchestrator

> Container lifecycle orchestration for cella.

Part of the [cella](../../README.md) workspace.

## Overview

cella-orchestrator extracts the shared container management logic so that both the CLI (`cella`) and daemon (`cella-daemon`) can call the same Rust functions instead of the daemon shelling out to CLI subprocesses. It owns the non-compose container-up pipeline, image resolution, lifecycle phase execution, tool installation, worktree-backed branch helpers, and worktree pruning helpers. All operations report progress through a channel-based `ProgressSender` that consumers render however they choose -- indicatif spinners in the CLI, serialized messages in the daemon.

## Architecture

### Key Types

- `UpConfig` -- borrowed configuration for the non-compose container-up pipeline (resolved config, image strategy, extra labels, host requirement policy, network rule policy)
- `ImageStrategy` -- how the up pipeline handles the container image: `Cached`, `Rebuild`, or `RebuildNoCache`
- `BranchConfig` -- configuration for creating a worktree-backed branch container (repo root, branch name, base ref, exec command)
- `PruneConfig` -- configuration for pruning merged worktrees (repo root, dry-run mode)
- `UpResult` / `UpOutcome` -- result of the container-up pipeline, including container ID, name, remote user, and what happened (Running, Started, Created)
- `BranchResult` -- result of branch creation with worktree path and container details
- `PruneResult` / `PrunedEntry` -- result of pruning with list of removed branches and errors
- `ExecResult` -- result of executing a command in a container (exit code)
- `WorktreeStatus` -- a worktree with its optional container status (branch, container name, state)
- `OrchestratorError` -- error type with variants for backend, git, config, container-exited, host-requirements, and other failures
- `ProgressSender` -- channel-based progress reporter with `step()`, `phase()`, `warn()`, `hint()`, and `println()` methods
- `ProgressEvent` -- enum of events (step started/completed/failed, phase started/completed, warnings, hints, output lines, errors)
- `StepHandle` / `PhaseHandle` / `PhaseChildHandle` -- RAII handles that send completion or failure events on finish or drop

### Modules

| Module | Purpose |
|--------|---------|
| `config` | Input configuration types (`UpConfig`, `ImageStrategy`, `HostRequirementPolicy`, `BranchConfig`, `PruneConfig`) |
| `result` | Output types (`UpResult`, `BranchResult`, `PruneResult`, `ExecResult`, `WorktreeStatus`) |
| `error` | `OrchestratorError` unified error type |
| `progress` | Channel-based progress reporting (`ProgressSender`, `ProgressEvent`, handle types) |
| `image` | Container image resolution: pull, build, and features layer |
| `lifecycle` | Lifecycle phase management: resolution, execution, and content tracking |
| `branch` | Worktree-backed branch creation and container delegation |
| `prune` | Detect and remove merged worktrees with their containers |
| `docker_helpers` | Docker container lookup and exec helpers via bollard API |

## Crate Dependencies

**Depends on:** [cella-backend](../cella-backend), [cella-env](../cella-env), [cella-features](../cella-features), [cella-git](../cella-git), [cella-network](../cella-network)

**Depended on by:** [cella-cli](../cella-cli)

## Testing

```sh
cargo test -p cella-orchestrator
```

Unit tests cover progress event emission, step/phase lifecycle (including drop-based failure reporting), and verbose mode gating.

## Development

The `ProgressSender` / `ProgressEvent` design decouples orchestrator logic from presentation. To add a new consumer (e.g., a web UI), receive `ProgressEvent` values from the channel and map them to your output format -- no changes to orchestrator code required.

When adding new orchestrator operations, accept a `ProgressSender` parameter and use `step()` / `phase()` to report progress. The RAII handles ensure that steps are always marked as completed or failed, even on early returns or panics.

Input configuration goes in `config.rs`, output types in `result.rs`. Keep orchestrator functions free of presentation concerns -- they should never reference indicatif, terminal formatting, or serialization formats directly.
