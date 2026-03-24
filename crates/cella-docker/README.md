# cella-docker

> Container runtime abstraction layer wrapping the Docker API.

Part of the [cella](../../README.md) workspace.

## Overview

cella-docker provides all container operations: creating, starting, stopping, and removing containers, building images, executing commands, uploading files, managing volumes and networks, and running lifecycle commands. It wraps the [bollard](https://docs.rs/bollard) Docker API client behind a `DockerApi` trait, enabling testability and future support for alternative runtimes (Podman, etc.).

The crate auto-detects the Docker socket location and connects to whatever Docker-compatible runtime is available (Docker Engine, OrbStack, Colima). Container naming, labeling, and image naming follow consistent conventions that allow cella to track and manage its containers.

### Spec Coverage

Implements the container lifecycle portions of the [Dev Container specification](https://containers.dev/implementors/json_reference/):
- Container creation with `image`, `build` (Dockerfile, context, args, target), `mounts`, `workspaceMount`
- Port configuration from `forwardPorts` and `portsAttributes`
- Lifecycle commands: `postCreateCommand`, `postStartCommand`, `postAttachCommand`, `updateContentCommand`
- `remoteUser`, `containerUser`, `updateRemoteUserUID`
- `userEnvProbe` for detecting the container user's environment
- `runArgs` — 30+ docker create flags (networking, resources, security, devices, GPU passthrough)
- `shutdownAction` — stored in container labels, gates `cella down` when "none"
- `waitFor` — lifecycle phase gating for `cella up` return
- `appPort` deprecation diagnostics

## Architecture

### Key Types

- `DockerApi` — trait abstracting all Docker operations (the extension point for new runtimes)
- `DockerClient` — bollard-backed implementation of `DockerApi`
- `ContainerInfo` — full container state including mounts, ports, labels
- `ContainerState` — lifecycle state tracking (created, running, stopped, etc.)
- `CreateContainerOptions` — container creation parameters
- `MountConfig` — mount specification (bind, volume, tmpfs)
- `ExecOptions` / `InteractiveExecOptions` — command execution configuration
- `ExecResult` — execution output with exit code
- `BuildOptions` — image build parameters
- `ParsedLifecycle` — parsed lifecycle command (string, array, or map form)
- `ContainerTarget` — resolved container identifier
- `FileToUpload` — file content for upload into containers

### Modules

| Module | Purpose |
|--------|---------|
| `client` | `DockerApi` trait definition and `DockerClient` (socket auto-detection, connection) |
| `docker_api_impl` | `DockerApi` trait implementation for `DockerClient` |
| `container` | `ContainerInfo`, `ContainerState`, status inspection |
| `config_map/` | `CreateContainerOptions`, `MountConfig` assembly from devcontainer config |
| `config_map/env` | Environment variable assembly (`containerEnv`, `remoteEnv`, forwarded vars) |
| `config_map/mounts` | Mount configuration (`mounts`, `workspaceMount`, bind mounts) |
| `config_map/ports` | Port binding configuration from `forwardPorts` and `portsAttributes` |
| `config_map/run_args` | `runArgs` parser — maps 30+ docker create flags to bollard HostConfig (networking, resources, security, devices, GPU) |
| `exec` | Command execution (interactive with PTY and detached) |
| `image` | Image building via Docker build API |
| `lifecycle` | Lifecycle command parsing and execution (`postCreate`, `postStart`, `postAttach`) |
| `names` | Container/image naming conventions and label management |
| `network` | Network creation and configuration |
| `volume` | Volume and mount management |
| `resolve` | Container resolution by ID, name, or label |
| `uid` | UID remapping for `updateRemoteUserUID` |
| `upload` | File upload to running containers via tar streaming |

## Crate Dependencies

**Depends on:** [cella-port](../cella-port), [cella-features](../cella-features)

**Depended on by:** [cella-cli](../cella-cli), [cella-compose](../cella-compose), [cella-doctor](../cella-doctor)

## Testing

```sh
cargo test -p cella-docker
```

Unit tests are inline within each module. Docker-dependent integration tests require a running Docker daemon and are marked with `#[ignore]`:

```sh
cargo test -p cella-docker -- --ignored
```

## Development

The `DockerApi` trait is the primary abstraction boundary. To add support for a new container runtime (e.g., Podman):
1. Implement the `DockerApi` trait for the new runtime
2. Add runtime detection in the client module
3. The rest of cella will work through the trait without changes

The `lifecycle` module handles the three forms of lifecycle commands in the spec: string (`"npm install"`), array (`["npm", "install"]`), and map (`{"vscode": "npm install"}`). When adding lifecycle support, ensure all three forms are handled.

Container naming uses the pattern `cella-{project}-{worktree}` with labels for tracking. The `names` module is the single source of truth for these conventions.
