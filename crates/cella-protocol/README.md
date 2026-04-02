# cella-protocol

> IPC wire format definitions for agent↔daemon and CLI↔daemon communication.

Part of the [cella](../../README.md) workspace.

## Overview

cella-protocol defines the message types and serialization format shared across cella's IPC boundaries. Three communication layers use this crate:

1. **Agent ↔ Daemon** (TCP) — the in-container agent reports port changes, credential requests, and browser opens to the host daemon; the daemon sends configuration, credential responses, and port mappings back. Worktree operations (branch, exec, prune, etc.) and background tasks are also routed through this channel.
2. **CLI ↔ Daemon** (Unix socket) — the CLI registers/deregisters containers, queries port status, and manages daemon lifecycle.
3. **Git credential helper** — key=value field parsing/formatting compatible with the git credential helper protocol.

Layers 1 and 2 use newline-delimited JSON. `AgentMessage`, `DaemonMessage`, `ManagementRequest`, and `ManagementResponse` are internally tagged with `#[serde(tag = "type", rename_all = "snake_case")]`. The handshake messages (`AgentHello`, `DaemonHello`) are plain structs without a `type` tag. Layer 3 (credential helper) uses `key=value` text, not JSON.

See [docs/specs/ipc-protocol.md](../../docs/specs/ipc-protocol.md) for the full protocol specification.

## Architecture

### Key Types

- `AgentHello` / `DaemonHello` — TCP handshake messages (protocol version, auth token, container identification)
- `AgentMessage` — messages from in-container agent to host daemon (port events, credentials, browser, worktree ops, tasks)
- `DaemonMessage` — messages from host daemon to in-container agent (ack, config, credential responses, operation results)
- `ManagementRequest` — CLI→daemon requests (register/deregister container, query ports/status, ping, shutdown)
- `ManagementResponse` — daemon→CLI responses (registration confirmation, port listing, status, errors)
- `PortProtocol` — transport protocol (Tcp, Udp)
- `BindAddress` — listener bind scope (Localhost, All)
- `OnAutoForward` — auto-forward behavior (Notify, OpenBrowser, Silent, Ignore, etc.)
- `PortAttributes` — per-port configuration (pattern, auto-forward behavior, label, protocol)
- `PortPattern` — port matching (Single, Range)
- `WorktreeOperationResult` / `DownOperationResult` / `TaskRunOperationResult` — operation result enums (Success or Error)
- `TaskEntry` — background task state (id, branch, container, status, command, elapsed)
- `WorktreeEntry` — worktree listing entry (branch, path, container association)
- `ForwardedPortDetail` — forwarded port info (container, ports, protocol, process, URL)
- `ContainerSummary` — registered container summary (name, id, port count, agent connection)

### Modules

| Module | Purpose |
|--------|---------|
| `lib` | All message enums, handshake structs, management protocol, and supporting types |
| `credential` | Git credential helper field parsing (`parse_credential_fields`) and formatting (`format_credential_fields`) |

## Crate Dependencies

**Depends on:** none (only serde, serde_json, thiserror)

**Depended on by:** [cella-port](../cella-port)

## Testing

```sh
cargo test -p cella-protocol
```

Unit tests cover message serialization round-trips, backward compatibility (missing optional fields), and credential field parsing/formatting.

## Development

This crate is a shared contract — changes to message types must be coordinated across both sides:

- **Host side:** cella-daemon reads `AgentMessage` and sends `DaemonMessage`; reads `ManagementRequest` and sends `ManagementResponse`
- **Container side:** cella-agent sends `AgentMessage` and reads `DaemonMessage`
- **CLI side:** cella-cli sends `ManagementRequest` and reads `ManagementResponse`

**Adding new enum variants is a breaking change** on all channels. Both the agent and daemon deserialize messages with `serde_json::from_str` — an unknown `"type"` value causes a deserialization error that tears down the connection. Adding optional fields to existing variants is safe when using `#[serde(default, skip_serializing_if = "Option::is_none")]`.

Version negotiation differs by channel:

- **Agent ↔ Daemon:** `PROTOCOL_VERSION` is exchanged in the `AgentHello`/`DaemonHello` handshake. New `AgentMessage`/`DaemonMessage` variants require a version bump.
- **CLI ↔ Daemon (management):** There is no version handshake. `ManagementRequest`/`ManagementResponse` changes require lockstep CLI/daemon rollout (both ship from the same binary release).
