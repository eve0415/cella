# cella-port

> Port allocation, detection, and IPC protocol for dev container port forwarding.

Part of the [cella](../../README.md) workspace.

## Overview

cella-port handles three aspects of port management: detecting listening ports inside containers, allocating host ports to avoid conflicts across concurrent containers, and defining the IPC protocol used between the host daemon and the in-container agent.

Port detection works by parsing `/proc/net/tcp` and `/proc/net/tcp6` inside the container to discover processes in the LISTEN state. The allocation table tracks which host ports are in use across all active containers to prevent collisions when multiple dev containers run simultaneously. The protocol module defines the message format for the daemon-agent communication channel.

### Spec Coverage

Supports the `forwardPorts` and `portsAttributes` properties from the [Dev Container specification](https://containers.dev/implementors/json_reference/).

## Architecture

### Key Types

- `ForwardedPort` — mapping between a container port and its allocated host port
- `PortAllocationTable` — tracks port assignments across multiple containers, resolves conflicts
- `DetectedListener` — a listening socket parsed from `/proc/net/tcp` (address, port, inode)
- `CellaPortError` — error type for port operations

### Modules

| Module | Purpose |
|--------|---------|
| `allocation` | `ForwardedPort`, `PortAllocationTable`, port range management (default range: 1024-65535) |
| `detection` | `/proc/net/tcp` parser, LISTEN state detection (TCP state code `0A`) |
| `protocol` | IPC message types (`AgentMessage`, `DaemonMessage`), serialization, state machine for daemon-agent communication |

## Crate Dependencies

**Depends on:** none (only serde, serde_json, thiserror)

**Depended on by:** [cella-docker](../cella-docker), [cella-agent](../cella-agent), [cella-daemon](../cella-daemon), [cella-cli](../cella-cli), [cella-doctor](../cella-doctor)

## Testing

```sh
cargo test -p cella-port
```

Unit tests cover `/proc/net/tcp` parsing with synthetic data via tempfile, port allocation conflict resolution, and protocol message round-trips.

## Development

This crate is a shared dependency across five consumers (cella-docker, cella-agent, cella-daemon, cella-cli, cella-doctor). The protocol module defines the wire format for daemon-agent communication — any changes to message types must be coordinated across both sides:

- **Host side:** cella-daemon reads `AgentMessage` and sends `DaemonMessage`
- **Container side:** cella-agent sends `AgentMessage` and reads `DaemonMessage`

The `/proc/net/tcp` parser handles both IPv4 (`/proc/net/tcp`) and IPv6 (`/proc/net/tcp6`). The LISTEN state is identified by TCP state code `0A` in the hex-encoded state field.
