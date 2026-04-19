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

## Crate Dependencies

**Depends on:** [cella-protocol](../cella-protocol)

**Depended on by:** [cella-agent](../cella-agent), [cella-daemon](../cella-daemon)

## Testing

```sh
cargo test -p cella-port
```

Unit tests cover `/proc/net/tcp` parsing with synthetic data via tempfile and port allocation conflict resolution.

The `/proc/net/tcp` parser handles both IPv4 (`/proc/net/tcp`) and IPv6 (`/proc/net/tcp6`). The LISTEN state is identified by TCP state code `0A` in the hex-encoded state field.
