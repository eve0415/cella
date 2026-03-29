# cella-container

> Apple Container backend for cella (experimental).

Part of the [cella](../../README.md) workspace.

## Overview

cella-container implements `cella_backend::ContainerBackend` by driving the Apple `container` CLI binary, part of the [Apple Containerization framework](https://developer.apple.com/documentation/containerization) introduced in macOS 26. This is an **experimental** backend — the CLI output format is pre-1.0 and may change between releases.

The backend discovers the `container` binary via `CELLA_CONTAINER_PATH` or `PATH` lookup, validates it by checking version output, and then shells out for all operations. JSON output parsing maps Apple's container metadata to cella's shared types.

### Requirements

- macOS 26 (Sequoia) or later
- Apple Silicon (arm64)
- Apple Container CLI installed

### Known Limitations

- **No Docker Compose support** — `ComposeBackend` is not implemented
- **No bridge networking** — only port forwarding is available
- **File injection via bind mount** — uses a host-side staging directory instead of native `cp` (Apple's CLI lacks container file copy)
- **Unsupported flags warn gracefully** — `--gpus`, `--privileged`, `--cap-add`, and similar Docker-specific flags emit warnings and are skipped
- **Native SSH forwarding** — uses `--ssh` flag instead of socket bind-mounting

## Architecture

### Key Types

- `AppleContainerBackend` — `ContainerBackend` implementation wrapping the Apple CLI
- `ContainerCli` — typed handle to the discovered `container` binary with async methods for each CLI subcommand
- `VersionInfo` — parsed version output for CLI validation
- `ContainerInspect` / `ContainerListEntry` — parsed JSON types from CLI output

### Modules

| Module | Purpose |
|--------|---------|
| `backend` | `ContainerBackend` trait implementation — maps cella operations to Apple CLI commands |
| `discovery` | CLI binary discovery via environment variable and PATH search, version validation |
| `sdk/` | Typed wrapper around the `container` CLI binary |
| `sdk/run` | Process spawning, output capture, and error handling for CLI invocations |
| `sdk/types` | Serde types for parsing CLI JSON output (inspect, list, version) |

## Crate Dependencies

**Depends on:** [cella-backend](../cella-backend)

**Depended on by:** [cella-cli](../cella-cli)

## Testing

```sh
cargo test -p cella-container
```

Unit tests cover CLI argument assembly, JSON output parsing, and container state mapping. Integration tests require macOS 26 with the Apple Container CLI installed and are gated behind the `integration-tests` feature.

## Development

The `sdk/` module is the typed interface to the Apple CLI. When Apple adds new subcommands or changes output format, update `sdk/types.rs` (serde structs) and the corresponding `ContainerCli` methods.

The `backend.rs` module maps `ContainerBackend` trait methods to `ContainerCli` calls. For unsupported Docker features, return `BackendError::NotSupported` with a clear message — the CLI layer handles these as warnings rather than hard failures.

File injection uses a bind-mounted staging directory (`~/.cache/cella/containers/<id>/`) because the Apple CLI does not support `container cp`. Files are written to the host staging directory before container creation, and the directory is bind-mounted into the container at the target path.
