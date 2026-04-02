# cella-docker

> Container runtime abstraction layer wrapping the Docker API.

Part of the [cella](../../README.md) workspace.

## Overview

cella-docker provides all Docker container operations: creating, starting, stopping, and removing containers, building images, executing commands, uploading files, managing volumes and networks, and running lifecycle commands. It implements `ContainerBackend` (from [cella-backend](../cella-backend)) using the [bollard](https://docs.rs/bollard) Docker API client. Internally, the `DockerApi` trait wraps bollard for testability.

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

- `DockerApi` — internal trait wrapping bollard Docker operations (not the extension point for new runtimes — see `ContainerBackend` in [cella-backend](../cella-backend))
- `DockerClient` — bollard-backed implementation of `DockerApi`, also implements `ContainerBackend`
- `ContainerInfo`, `ContainerState`, `CreateContainerOptions`, `MountConfig`, `ExecOptions`, `InteractiveExecOptions`, `ExecResult`, `BuildOptions` — shared types defined in [cella-backend](../cella-backend) and re-exported here
- `ParsedLifecycle` — parsed lifecycle command (string, array, or map form)
- `ContainerTarget` — resolved container identifier
- `FileToUpload` — file content for upload into containers

### Modules

| Module | Purpose |
|--------|---------|
| `client` | `DockerApi` trait definition and `DockerClient` (socket auto-detection, connection) |
| `docker_api_impl` | `DockerApi` trait implementation for `DockerClient` |
| `container` | `ContainerInfo`, `ContainerState`, status inspection |
| `config_map/` | `CreateContainerOptions`, `MountConfig` assembly from devcontainer config (mounts, ports, run_args consolidated here) |
| `config_map/env` | Environment variable assembly (`containerEnv`, `remoteEnv`, forwarded vars) |
| `discovery` | Docker socket auto-discovery (Colima, Podman, Rancher Desktop, standard paths) |
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

**Depends on:** [cella-backend](../cella-backend)

**Depended on by:** [cella-cli](../cella-cli), [cella-doctor](../cella-doctor)

## Testing

```sh
cargo test -p cella-docker
```

Unit tests are inline within each module. Docker-dependent integration tests require a running Docker daemon and are marked with `#[ignore]`:

```sh
cargo test -p cella-docker -- --ignored
```

## Development

To add support for a new container runtime (e.g., Podman), implement `ContainerBackend` from [cella-backend](../cella-backend) in a new crate — see that crate's README for the full guide. `DockerApi` is an internal abstraction within this crate for wrapping bollard, not the extension point for new runtimes.

The `lifecycle` module handles the three forms of lifecycle commands in the spec: string (`"npm install"`), array (`["npm", "install"]`), and map (`{"vscode": "npm install"}`). When adding lifecycle support, ensure all three forms are handled.

Container naming uses the pattern `cella-{project}-{worktree}` with labels for tracking. The `names` module is the single source of truth for these conventions.
