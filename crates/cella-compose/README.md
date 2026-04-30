# cella-compose

> Docker Compose orchestration for devcontainer projects.

Part of the [cella](../../README.md) workspace.

## Overview

cella-compose handles devcontainer projects that use `dockerComposeFile` instead of a single `image` or `Dockerfile`. When `cella up` detects a Docker Compose configuration, it delegates to this crate for the full orchestration flow.

The crate generates override compose files that layer cella's customizations (mounts, environment variables, labels, entrypoint) on top of the user's compose files, then shells out to the `docker compose` V2 CLI for actual container management. Multi-file change detection via SHA-256 hashing enables rebuild-on-change: if any compose file changes between invocations, cella re-builds the project.

Container discovery uses Docker labels to find compose-managed containers after startup. The crate validates the compose YAML minimally (enough to find the primary service and detect errors early) rather than fully parsing Docker Compose's schema.

### Spec Coverage

Implements the Docker Compose portions of the [Dev Container specification](https://containers.dev/implementors/json_reference/):
- `dockerComposeFile` (single or array of files)
- `service` (primary service selection)
- `runServices` (services to start alongside the primary)
- `shutdownAction` (stopCompose or none)
- `overrideCommand` (entrypoint override)
- `workspaceFolder` mount into the primary service

## Architecture

### Key Types

- `ComposeProject` — orchestrates a compose-based devcontainer (start, stop, discover containers)
- `ComposeCommand` — wraps the `docker compose` CLI (up, down, ps, build, exec)
- `OverrideConfig` — configuration for generating the cella override compose file
- `ShutdownAction` — what to do on `cella down` (stop compose project or do nothing)
- `CellaComposeError` — error type for compose operations

### Modules

| Module | Purpose |
|--------|---------|
| `cli` | `ComposeCommand` — shells out to `docker compose` V2 with project/file arguments |
| `discovery` | Finds compose-managed containers by Docker labels after `docker compose up` |
| `error` | `CellaComposeError` enum |
| `hash` | Multi-file SHA-256 hashing for compose config change detection |
| `override_file` | Generates cella override YAML (mounts, env vars, labels, entrypoint, capabilities) |
| `config` | Typed resolution of `docker compose config` output (variable substitution, service build/image info) |
| `dockerfile` | Dockerfile reading, stage naming, and combined Dockerfile generation for compose + features support |
| `parse` | Minimal YAML parsing — validates structure, extracts service names and primary service |
| `project` | `ComposeProject` lifecycle — initialization, startup, container resolution, shutdown |

## Crate Dependencies

**Depends on:** [cella-backend](../cella-backend)

**Depended on by:** [cella-cli](../cella-cli), [cella-orchestrator](../cella-orchestrator)

## Testing

```sh
cargo test -p cella-compose
```

Tests use insta snapshot assertions for override file generation and tempfile for YAML parsing. After modifying override generation:

```sh
cargo insta review
```

## Development

The override file is the core mechanism: cella writes a temporary compose override YAML that adds its labels, mounts, environment variables, and entrypoint to the user's primary service. This override is passed to `docker compose -f <user-files> -f <override>` so that the user's configuration is preserved while cella adds its management layer.

The `docker compose` CLI is invoked as a subprocess, not via API. This means compose V2 must be installed on the host. The `ComposeCommand` type handles project naming and file argument assembly.

Change detection hashes all compose files (including the override) to determine if `docker compose build` needs to re-run. The hash is stored as a container label and compared on subsequent `cella up` invocations.
