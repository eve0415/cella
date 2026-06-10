# cella-container

> Apple Container backend for cella.

Part of the [cella](../../README.md) workspace.

## Overview

cella-container implements `cella_backend::ContainerBackend` by driving the Apple `container` CLI binary, part of the [Apple Containerization framework](https://developer.apple.com/documentation/containerization). It targets the stable 1.0.0 CLI surface and structured-output shapes.

The backend discovers the `container` binary via `CELLA_CONTAINER_PATH` or `PATH` lookup, validates it with `container system version`, and rejects releases older than 1.0.0 (their JSON output is incompatible). All operations shell out to the binary; JSON output parsing maps Apple's container metadata to cella's shared types.

### Requirements

- Apple Container CLI **1.0.0 or newer**
- macOS 26 for full functionality (macOS 15 runs with reduced networking)
- Apple Silicon (arm64)

### Feature Support

- **Lifecycle, exec, logs, build, pull** — full support
- **File injection** — native `container cp` plus an exec to normalize ownership/mode
- **Networks** — the shared `cella` network and per-workspace `cella-net-*` networks are created with the same labels and names as the Docker backend. vmnet fixes attachments at creation, so they are requested on `container create`; post-create `network connect` does not exist. Requires macOS 26 (on macOS 15 the backend degrades to the default network).
- **runArgs** — `--cap-add`, memory/cpus (whole vCPUs), `--shm-size`, `--init`, DNS options, ulimits, tmpfs, labels and `--runtime` map natively; unsupported flags (`--privileged`, `--security-opt`, devices, GPUs, namespaces, restart policies, ...) emit one consolidated warning
- **SSH agent** — native `--ssh` forwarding when `SSH_AUTH_SOCK` is set

### Known Limitations

- **No Docker Compose support** — `capabilities().compose` is `false`; Apple Container has no compose equivalent
- **No managed agent** — `capabilities().managed_agent` is `false`. The cella daemon listens on the host loopback only, and Apple Container has no automatic `host.docker.internal` equivalent: the vmnet gateway cannot reach loopback-bound services, and Apple's `container system dns create <domain> --localhost <ip>` workaround requires sudo, disables iCloud Private Relay, and does not survive a reboot. Enabling the agent needs a daemon bind-address decision first (see `docs/specs/container-backends.md`).
- **Host access needs one-time setup** — `host_gateway()` returns `host.container.internal`, which resolves only after `sudo container system dns create host.container.internal --localhost <ip>`
- **No exit codes** — `container ls`/`inspect` do not report exit codes, so `ContainerInfo::exit_code` is `None`
- **Anonymous volumes are never auto-removed** — unlike Docker, Apple Container keeps them until `container volume rm`

## Architecture

### Key Types

- `AppleContainerBackend` — `ContainerBackend` implementation wrapping the Apple CLI
- `ContainerCli` — typed handle to the discovered `container` binary with async methods for each CLI subcommand
- `DiscoveryError` — why discovery rejected a binary (missing, foreign, or pre-1.0)
- `ContainerListEntry` / `NetworkListEntry` — parsed JSON types from CLI output

### Modules

| Module | Purpose |
|--------|---------|
| `backend` | `ContainerBackend` trait implementation — maps cella operations to Apple CLI commands |
| `discovery` | CLI binary discovery via environment variable and PATH search, 1.0.0 version gate |
| `sdk/` | Typed wrapper around the `container` CLI binary |
| `sdk/run` | Process spawning, output capture, and error handling for CLI invocations |
| `sdk/types` | Serde types for parsing CLI JSON output (inspect, list, network, version) |

## Crate Dependencies

**Depends on:** [cella-backend](../cella-backend)

**Depended on by:** [cella-cli](../cella-cli)

## Testing

```sh
cargo test -p cella-container
```

Unit tests cover CLI argument assembly, JSON output parsing, and container state mapping. Integration tests require macOS with the Apple Container CLI installed and use `#[runtime_test(apple_container)]` for runtime detection — they skip gracefully on other platforms.

## Development

The `sdk/` module is the typed interface to the Apple CLI. When Apple adds new subcommands or changes output format, update `sdk/types.rs` (serde structs) and the corresponding `ContainerCli` methods.

The `backend.rs` module maps `ContainerBackend` trait methods to `ContainerCli` calls. For unsupported Docker features, return `BackendError::NotSupported` with a clear message — the CLI layer handles these as warnings rather than hard failures.
