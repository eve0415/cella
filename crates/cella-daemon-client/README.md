# cella-daemon-client

> Async client for the cella daemon management Unix socket.

Part of the [cella](../../README.md) workspace.

## Overview

cella-daemon-client provides `DaemonClient`, an async client that communicates with a running cella daemon over its Unix management socket. The wire protocol is newline-delimited JSON: the client serializes a `ManagementRequest`, writes it as a single line, and reads back a `ManagementResponse`. All request/response types come from `cella-protocol`.

The client exposes typed methods for each daemon operation: health checks (`ping`), status queries, container registration/deregistration, IP updates, port queries, SSH-agent proxy management, and daemon shutdown. Each method sends the appropriate request variant and maps the response to a domain-specific return type, converting unexpected response variants into `DaemonClientError`.

The `ssh_proxy` module bridges the orchestrator's SSH-agent forwarding needs into daemon RPC calls. On colima and other non-native Docker runtimes, bind-mounting Unix sockets fails because virtiofs rejects `mkdir` on host-side socket paths. The module works around this by registering a daemon-managed TCP bridge instead, and translating the bridge details into the environment variables (`SSH_AUTH_SOCK`, `CELLA_SSH_AGENT_BRIDGE`, `CELLA_SSH_AGENT_TARGET`) that the in-container agent expects.

## Architecture

### Key Types

- `DaemonClient` -- holds a socket path, provides typed async methods for every management RPC
- `DaemonStatus` -- structured status response (PID, uptime, container list, control port, OrbStack detection, hostname proxy state)
- `SshAgentProxyRegistration` -- bridge port and refcount returned after registering an SSH-agent proxy
- `DaemonClientError` -- error enum covering connection failures, I/O errors, protocol (de)serialization errors, daemon-side errors, and unexpected response variants
- `ResolvedSshProxy` -- env vars, bridge port, and refcount produced by a successful SSH-agent proxy registration
- `send_management_request` -- standalone function for one-shot request/response over a Unix socket (used internally by `DaemonClient`)

### Modules

| Module | Purpose |
|--------|---------|
| `lib` | `DaemonClient`, `DaemonStatus`, `SshAgentProxyRegistration`, `DaemonClientError`, and the low-level `send_management_request` function |
| `ssh_proxy` | SSH-agent TCP-bridge registration/release via daemon RPC; translates proxy requests into container env vars |

## Crate Dependencies

**Depends on:** [cella-env](../cella-env), [cella-protocol](../cella-protocol)

**Depended on by:** [cella-cli](../cella-cli), [cella-compose](../cella-compose), [cella-daemon](../cella-daemon) (dev), [cella-doctor](../cella-doctor), [cella-orchestrator](../cella-orchestrator)

## Testing

```sh
cargo test -p cella-daemon-client
```

Unit tests in `ssh_proxy` use a mock daemon (a `UnixListener` that replies with canned `ManagementResponse` variants) to verify:

- Happy-path registration produces the correct env vars and bridge port
- Daemon error responses surface as `None` so the orchestrator skips forwarding
- Unexpected response variants surface as `None`
- Unreachable daemon sockets surface as `None` (not a panic)
- Release sends the correct request shape and tolerates an unreachable daemon

## Development

The client is intentionally thin -- it maps 1:1 to `ManagementRequest`/`ManagementResponse` variants defined in `cella-protocol`. To add a new daemon RPC:

1. Add the request/response variants in `cella-protocol`
2. Add a typed method on `DaemonClient` that sends the request and pattern-matches the response
3. Return a domain-specific type or `DaemonClientError` for unexpected variants
