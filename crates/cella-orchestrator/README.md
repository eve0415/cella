# cella-orchestrator

> Container lifecycle orchestration for cella.

Part of the [cella](../../README.md) workspace.

## Overview

cella-orchestrator extracts the shared container management logic so that both the CLI (`cella`) and daemon (`cella-daemon`) can call the same Rust functions instead of the daemon shelling out to CLI subprocesses. It owns the single-container up pipeline, image resolution, host requirements validation, shell detection, environment caching, credential protection, worktree-backed branch helpers, and pruning. Compose orchestration, container setup, lifecycle phase execution, progress reporting, and UID-remap image logic live in `cella-backend` and `cella-compose`; this crate re-exports them for backward compatibility. All operations go through the `cella-backend` abstraction -- no direct Docker API dependency.

## Architecture

### Key Types

- `UpConfig` -- borrowed configuration for the single-container up pipeline (resolved config, image strategy, extra labels, host requirement policy, network rule policy)
- `UpHooks` (trait) -- callback interface for host-specific operations during container-up (daemon env, agent sync, start/stop notifications)
- `ImageStrategy` -- how the up pipeline handles the container image: `Cached`, `Rebuild`, or `RebuildNoCache`
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

### Modules

| Module | Purpose |
|--------|---------|
| `up` | Main single-container up pipeline with `UpHooks` trait for host integration |
| `config` | Input configuration types (`UpConfig`, `ImageStrategy`, `HostRequirementPolicy`, `BranchConfig`, `PruneConfig`) |
| `result` | Output types (`UpResult`, `BranchResult`, `PruneResult`, `ExecResult`, `WorktreeStatus`) |
| `error` | `OrchestratorError` unified error type |
| `image` | Container image resolution: pull, build, and features layer |
| `env_cache` | User environment probing and caching (`~/.cella/probed-env.json`) |
| `host_requirements` | CPU/memory/storage/GPU validation against `hostRequirements` |
| `shell_detect` | Shell detection and quoting for container exec |
| `branch` | Worktree-backed branch creation and container delegation |
| `prune` | Detect and remove merged worktrees with their containers |
| `docker_helpers` | Container lookup and exec helpers via `ContainerBackend` trait |
| `daemon_registration` | Builds daemon container-registration payloads from devcontainer config or existing container labels |
| `approved_providers` | Persistent storage for user-approved custom credential providers with tamper detection |
| `credential_protect` | Phantom token generation and daemon registration for credential protection |

Re-exported from other crates (backward compatibility):

| Re-export | Source crate | Purpose |
|-----------|-------------|---------|
| `compose_build` | cella-compose | Docker Compose build pipeline with feature resolution |
| `compose_features` | cella-compose | Combined-Dockerfile generation for Compose + Features |
| `compose_mounts` | cella-compose | Mount parity helpers for Compose services |
| `compose_up` | cella-compose | Docker Compose container-up pipeline with `ComposeUpHooks` trait |
| `config_map` | cella-config | Maps devcontainer.json to `CreateContainerOptions` |
| `container_setup` | cella-backend | Post-creation container setup (user resolution, lifecycle commands, SSH/git injection) |
| `lifecycle` | cella-backend | Lifecycle phase management: resolution, execution, and content tracking |
| `progress` | cella-backend | Channel-based progress reporting (`ProgressSender`, `ProgressEvent`, handle types) |
| `uid_image` | cella-backend | Build-time UID remap layer (`Dockerfile.uid-remap`) |
| `tool_install` | cella-tool-install | AI coding tool installation (Claude Code, Codex, Gemini CLI) |
| `ssh_proxy_client` | cella-daemon-client | SSH proxy client for tunneled connections |

## Crate Dependencies

**Depends on:** [cella-backend](../cella-backend), [cella-compose](../cella-compose), [cella-config](../cella-config), [cella-daemon-client](../cella-daemon-client), [cella-env](../cella-env), [cella-features](../cella-features), [cella-git](../cella-git), [cella-network](../cella-network), [cella-protocol](../cella-protocol), [cella-tool-install](../cella-tool-install)

**Depended on by:** [cella-cli](../cella-cli)

## Testing

```sh
cargo test -p cella-orchestrator
```

Unit tests cover approved-provider tamper detection, credential-protect token generation, and host requirements validation.

## Development

Input configuration goes in `config.rs`, output types in `result.rs`. Keep orchestrator functions free of presentation concerns -- they should never reference indicatif, terminal formatting, or serialization formats directly. Progress reporting (`ProgressSender`) now lives in `cella-backend` and is re-exported here.
