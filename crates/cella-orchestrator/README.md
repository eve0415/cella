# cella-orchestrator

> Container lifecycle orchestration for cella.

Part of the [cella](../../README.md) workspace.

## Overview

cella-orchestrator extracts the shared container management logic so that both the CLI (`cella`) and daemon (`cella-daemon`) can call the same Rust functions instead of the daemon shelling out to CLI subprocesses. It owns the full container-up pipeline for both single-container and Docker Compose workflows, image resolution, lifecycle phase execution, host requirements validation, shell detection, AI tool installation (Claude Code, Codex, Gemini), environment caching, config-to-container mapping, worktree-backed branch helpers, and pruning. All operations go through the `cella-backend` abstraction -- no direct Docker API dependency. Progress is reported through a channel-based `ProgressSender` that consumers render however they choose.

## Architecture

### Key Types

- `UpConfig` -- borrowed configuration for the non-compose container-up pipeline (resolved config, image strategy, extra labels, host requirement policy, network rule policy)
- `UpHooks` (trait) -- callback interface for host-specific operations during container-up (daemon env, agent sync, start/stop notifications)
- `ImageStrategy` -- how the up pipeline handles the container image: `Cached`, `Rebuild`, or `RebuildNoCache`
- `ComposeUpConfig` -- configuration for the Compose container-up pipeline
- `ComposeUpResult` / `ComposeUpOutcome` -- result of the Compose pipeline (Created or Running)
- `ComposeUpHooks` (trait) -- callback interface for Compose-specific operations (container registration, agent launch, post-create setup)
- `PruneHooks` (trait) -- callback interface for pruning operations (container deregistration, compose down, daemon cleanup)
- `NoOpHooks` -- no-op implementation of `UpHooks` for testing
- `BranchConfig` -- configuration for creating a worktree-backed branch container (repo root, branch name, base ref, exec command)
- `PruneConfig` -- configuration for pruning merged worktrees (repo root, dry-run mode)
- `UpResult` / `UpOutcome` -- result of the container-up pipeline, including container ID, name, remote user, and what happened (Running, Started, Created)
- `BranchResult` -- result of branch creation with worktree path and container details
- `PruneResult` / `PrunedEntry` -- result of pruning with list of removed branches and errors
- `ExecResult` -- result of executing a command in a container (exit code)
- `WorktreeStatus` -- a worktree with its optional container status (branch, container name, state)
- `RequirementCheck` / `ValidationResult` -- host requirements validation results
- `OrchestratorError` -- error type with variants for backend, git, config, container-exited, host-requirements, and other failures
- `ProgressSender` -- channel-based progress reporter with `step()`, `phase()`, `warn()`, `hint()`, and `println()` methods
- `ProgressEvent` -- enum of events (step started/completed/failed, phase started/completed, warnings, hints, output lines, errors)
- `StepHandle` / `PhaseHandle` / `PhaseChildHandle` -- RAII handles that send completion or failure events on finish or drop

### Modules

| Module | Purpose |
|--------|---------|
| `up` | Main container-up pipeline with `UpHooks` trait for host integration |
| `compose_build` | Docker Compose build pipeline with feature resolution |
| `compose_features` | Combined-Dockerfile generation for Compose + Features |
| `compose_mounts` | Mount parity helpers for Compose services (workspace bind, SSH-agent forward, cella-agent volume) so compose containers match single-container behavior |
| `compose_up` | Docker Compose container-up pipeline with `ComposeUpHooks` trait |
| `config` | Input configuration types (`UpConfig`, `ImageStrategy`, `HostRequirementPolicy`, `BranchConfig`, `PruneConfig`) |
| `config_map` | Maps devcontainer.json to `CreateContainerOptions` (submodules: env, mounts, ports, run_args) |
| `container_setup` | Post-creation container setup (user resolution, lifecycle commands, SSH/git injection) |
| `result` | Output types (`UpResult`, `BranchResult`, `PruneResult`, `ExecResult`, `WorktreeStatus`) |
| `error` | `OrchestratorError` unified error type |
| `progress` | Channel-based progress reporting (`ProgressSender`, `ProgressEvent`, handle types) |
| `image` | Container image resolution: pull, build, and features layer |
| `lifecycle` | Lifecycle phase management: resolution, execution, and content tracking |
| `env_cache` | User environment probing and caching (`~/.cella/probed-env.json`) |
| `host_requirements` | CPU/memory/storage/GPU validation against `hostRequirements` |
| `shell_detect` | Shell detection and quoting for container exec |
| `tool_install` | AI coding tool installation (Claude Code, Codex, Gemini CLI); self-heals PATH and reports install status from on-disk reality rather than optimistic success |
| `uid_image` | Build-time UID remap layer (`Dockerfile.uid-remap`) that matches the devcontainer CLI's `updateUID.Dockerfile` approach |
| `branch` | Worktree-backed branch creation and container delegation |
| `prune` | Detect and remove merged worktrees with their containers |
| `docker_helpers` | Container lookup and exec helpers via `ContainerBackend` trait |

## Crate Dependencies

**Depends on:** [cella-backend](../cella-backend), [cella-compose](../cella-compose), [cella-config](../cella-config), [cella-env](../cella-env), [cella-features](../cella-features), [cella-git](../cella-git), [cella-network](../cella-network), [cella-port](../cella-port)

**Depended on by:** [cella-cli](../cella-cli), [cella-doctor](../cella-doctor)

## Testing

```sh
cargo test -p cella-orchestrator
```

Unit tests cover progress event emission, step/phase lifecycle (including drop-based failure reporting), and verbose mode gating.

## Development

The `ProgressSender` / `ProgressEvent` design decouples orchestrator logic from presentation. To add a new consumer (e.g., a web UI), receive `ProgressEvent` values from the channel and map them to your output format -- no changes to orchestrator code required.

When adding new orchestrator operations, accept a `ProgressSender` parameter and use `step()` / `phase()` to report progress. The RAII handles ensure that steps are always marked as completed or failed, even on early returns or panics.

Input configuration goes in `config.rs`, output types in `result.rs`. Keep orchestrator functions free of presentation concerns -- they should never reference indicatif, terminal formatting, or serialization formats directly.
