# cella-backend

> Container backend trait definitions and shared types.

Part of the [cella](../../README.md) workspace.

## Overview

cella-backend defines the `ContainerBackend` trait — the extension point for adding new container runtimes to cella. All backend-agnostic code works against this trait, and each runtime (Docker, Apple Container) provides its own implementation. The crate also houses all shared types (`ContainerInfo`, `ContainerState`, `CreateContainerOptions`, etc.), backend capability flags, and the unified error type `BackendError`.

Container and image naming conventions live here so that all backends use consistent naming and labeling, regardless of the underlying runtime.

## Architecture

### Key Types

- `ContainerBackend` — core trait defining async container lifecycle, exec, image, networking, and agent operations. Uses `BoxFuture` for object safety (`dyn ContainerBackend`)
- `BackendCapabilities` — backend capability flags (`compose`, `managed_agent`)
- `BackendKind` — enum (`Docker`, `AppleContainer`) identifying which backend is in use
- `BackendError` — unified error type with variants for container-not-found, image-not-found, build failures, exec failures, unsupported operations, and runtime-specific errors
- `ContainerInfo` — full container state including ID, name, state, ports, mounts, labels, image
- `ContainerState` — lifecycle state (Running, Stopped, Created, Removing, Other)
- `CreateContainerOptions` — container creation parameters (image, env, mounts, ports, run args overrides)
- `ExecOptions` / `InteractiveExecOptions` — command execution configuration
- `ExecResult` — execution output with exit code, stdout, stderr
- `ImageDetails` — image metadata (user, env vars, devcontainer metadata label)
- `BuildOptions` — image build parameters (context, dockerfile, tag, build args, target)
- `MountConfig` / `MountInfo` — mount specification and inspection types
- `PortBinding` / `PortForward` — port mapping types
- `DeviceSpec` / `UlimitSpec` / `GpuRequest` / `RunArgsOverrides` — container resource configuration

### Modules

| Module | Purpose |
|--------|---------|
| `traits` | `ContainerBackend`, `BackendCapabilities`, and `BoxFuture` |
| `types` | All shared types (`BackendKind`, `ContainerInfo`, `ContainerState`, `CreateContainerOptions`, etc.) |
| `names` | Container/image naming conventions and label generation (consistent across all backends) |
| `error` | `BackendError` unified error type |

## Crate Dependencies

**Depends on:** none (foundation crate — only `sha2`, `hex`, `chrono`, `thiserror`)

**Depended on by:** [cella-cli](../cella-cli), [cella-container](../cella-container), [cella-docker](../cella-docker), [cella-orchestrator](../cella-orchestrator)

## Testing

```sh
cargo test -p cella-backend
```

Unit tests cover naming conventions and container state parsing.

## Development

To add a new container backend:
1. Create a new crate that depends on `cella-backend`
2. Implement `ContainerBackend` for your runtime
3. Implement `capabilities()` to advertise optional features like compose and managed agent support
4. Add backend detection and selection in `cella-cli/src/backend.rs`

The `ContainerBackend` trait uses `BoxFuture` return types for object safety. This means all methods return `Pin<Box<dyn Future>>` rather than using `async fn`, enabling callers to work with `dyn ContainerBackend` trait objects without knowing the concrete backend type.

`BackendError::NotSupported` should be returned for operations that a backend cannot perform (e.g., Apple Container does not support `--privileged`). The CLI handles these gracefully by warning the user rather than failing hard.
