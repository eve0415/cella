# cella-daemon

> Unified host-side daemon for port forwarding, credential proxying, clipboard forwarding, SSH agent bridging, reverse tunnelling, and browser handling.

Part of the [cella](../../README.md) workspace.

## Overview

cella-daemon is the host-side counterpart to the in-container cella-agent. It runs as a background process on the host and provides six services:

1. **Port management** — receives port detection events from in-container agents and sets up host-side port forwarding
2. **Credential proxying** — forwards git credential requests from containers to the host's credential store
3. **Browser handling** — receives browser-open requests from containers and opens URLs in the host's default browser (enabling OAuth callbacks and similar flows)
4. **Clipboard forwarding** — bidirectional clipboard sync between host and containers (copy/paste via platform-native backends)
5. **SSH agent bridging** — TCP bridge from the host's `$SSH_AUTH_SOCK` into containers, avoiding virtiofs bind-mount issues
6. **Reverse tunnelling** — broker that matches pending tunnel requests with incoming agent tunnel connections

The daemon consolidates what was previously a standalone credential proxy into a single process. It manages PID files for lifecycle tracking, generates authentication tokens for agent connections, and includes health monitoring to detect stale connections.

The daemon has special handling for OrbStack, which provides its own port forwarding mechanism that needs to be coordinated with rather than duplicated.

## Architecture

### Key Types

- `CellaDaemonError` — error type for daemon operations

### Modules

| Module | Purpose |
|--------|---------|
| `daemon` | Main daemon entry point — PID file management, control server setup, health monitor spawning, auth token generation |
| `control_server` | Accepts and manages TCP connections from in-container agents |
| `management` | Daemon lifecycle API (start, stop, status, ensure-running) |
| `port_manager` | Processes port detection events from agents, manages host-side forwarding |
| `browser` | Handles browser-open requests from agents |
| `credential` | Forwards credential requests from agents to host credential stores |
| `proxy` | Credential proxy forwarding logic |
| `health` | Monitors agent connections, detects stale/dead connections |
| `orbstack` | OrbStack-specific port handling (coordinates with OrbStack's built-in forwarding) |
| `shared` | Shared daemon primitives — PID management, process checks, socket helpers |
| `stream_bridge` | Per-exec TCP stream bridge for TTY forwarding (interactive shell sessions through the daemon) |
| `task_manager` | Background task manager for in-container worktree operations (tracks exec handles, output, lifecycle state) |
| `clipboard` | Bidirectional clipboard forwarding — platform-native backends (pbcopy/xsel/xclip/wl-clipboard) for copy and paste |
| `ssh_proxy` | Per-workspace SSH-agent TCP bridge — forwards the host's `$SSH_AUTH_SOCK` over TCP so containers get a working `SSH_AUTH_SOCK` without bind mounts |
| `tunnel` | Reverse-tunnel broker — matches pending tunnel requests with incoming agent tunnel connections |
| `logging` | File-based tracing to `~/.cella/daemon.log` with size rotation |

## Crate Dependencies

**Depends on:** [cella-port](../cella-port), [cella-protocol](../cella-protocol)

**Depended on by:** [cella-cli](../cella-cli), [cella-doctor](../cella-doctor), [cella-orchestrator](../cella-orchestrator)

## Testing

```sh
cargo test -p cella-daemon
```

Unit tests use tempfile for PID file and socket path management. Integration testing requires a running container with the cella-agent.

## Development

The daemon communicates with agents using the protocol defined in `cella_protocol`. Any changes to message types must be coordinated across cella-daemon, cella-agent, and cella-protocol.

Key runtime files:
- **PID file** — tracks whether the daemon is running
- **Socket path** — Unix socket for local control
- **Port file** — stores the TCP port for agent connections
- **Control socket** — for management commands (start/stop/status)

The OrbStack module is important: OrbStack provides its own port forwarding, so the daemon must detect OrbStack and avoid setting up duplicate forwarding. This is the main source of runtime-specific complexity in this crate.
