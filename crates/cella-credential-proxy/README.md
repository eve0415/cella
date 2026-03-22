# cella-credential-proxy

> Git credential forwarding proxy over Unix socket and TCP.

Part of the [cella](../../README.md) workspace.

## Overview

cella-credential-proxy runs as a daemon on the host, listening on both a Unix socket and a TCP port. When a git credential request arrives from inside a container, the proxy forwards it to the host's git credential store (or gh CLI, or other credential helpers) and returns the result.

The daemon features automatic idle timeout (30 minutes of inactivity) and periodic health checks (every 5 minutes). PID file management ensures only one instance runs at a time.

Two transport modes support different container runtime topologies:
- **Unix socket** — for runtimes with direct filesystem access (Linux native, Docker Desktop on Mac with socket mounting)
- **TCP** — for VM-based runtimes where socket bind-mounting doesn't work (OrbStack, Colima), using `host.docker.internal` for connectivity

> **Note:** This crate's functionality is being consolidated into cella-daemon, which provides a unified daemon for credentials, port forwarding, and browser handling. This crate may be deprecated in a future release.

## Architecture

### Key Types

- `CellaCredentialProxyError` — error type for proxy operations

### Modules

| Module | Purpose |
|--------|---------|
| `daemon` | Daemon entry point — PID file management, Unix socket + TCP listener setup, idle timeout monitoring |
| `server` | Credential request handling and response |
| `client` | Client-side connection management for sending credential requests |
| `protocol` | Wire protocol for credential request/response serialization |
| `host` | Host-side credential resolution (invokes the host's git credential helpers) |

## Crate Dependencies

**Depends on:** none (only tokio, thiserror, tracing)

**Depended on by:** [cella-cli](../cella-cli)

## Testing

```sh
cargo test -p cella-credential-proxy
```

Unit tests use tempfile for PID file and socket path testing. Protocol serialization round-trip tests verify the wire format.

## Development

The credential protocol follows git's credential helper protocol (key-value pairs separated by newlines). The proxy translates between this text protocol and its internal message format.

The idle timeout is important for resource management — the proxy shuts itself down after 30 minutes of no activity rather than running indefinitely. The health check interval (5 minutes) monitors connection liveness.
