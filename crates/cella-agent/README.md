# cella-agent

> In-container agent for port detection, proxying, and credential forwarding.

Part of the [cella](../../README.md) workspace.

## Overview

cella-agent is a binary that runs inside dev containers started by cella. It is automatically uploaded into containers during `cella up` and handles these responsibilities:

1. **Port detection** — polls `/proc/net/tcp` for new listeners and reports them to the host daemon for automatic port forwarding
2. **Port proxying** — rebinds localhost-bound listeners to `0.0.0.0` via `port_proxy` so they are reachable from outside the container
3. **Forward proxy** — outbound HTTP/CONNECT proxy on `127.0.0.1` for egress filtering, enforcing network blocking rules via `forward_proxy`
4. **MITM proxy** — HTTPS interception proxy for path-level blocking rules (works alongside the forward proxy via `mitm`)
5. **Browser interception** — handles `BROWSER` environment variable calls, forwarding URL open requests to the host (enables OAuth callbacks)
6. **Credential forwarding** — forwards git credential requests to the host daemon for transparent authentication
7. **Plugin synchronization** — synchronizes editor plugin/extension manifests between host and container
8. **CLI mode** — when invoked as `cella` (via symlink, `cella` -> `cella-agent`), provides in-container CLI commands that delegate to the host daemon

The agent communicates with the host-side cella-daemon over a TCP control connection. If the daemon is unavailable, it falls back to standalone mode (port watching only, no forwarding).

The binary uses manual argument parsing instead of clap to minimize binary size, since it ships inside every container.

## Commands

```
cella-agent daemon [--poll-interval <ms>]    # Run the agent daemon (default: 1000ms)
cella-agent browser-open <url>               # Open a URL on the host
cella-agent credential <operation>           # Handle git credential request (get/store/erase)
```

## Architecture

### Key Types

- `AgentCommand` — enum of the three command modes (Daemon, BrowserOpen, Credential)
- `CellaAgentError` — error type for agent operations

### Modules

| Module | Purpose |
|--------|---------|
| `port_watcher` | Polls `/proc/net/tcp` on an interval, detects new/removed listeners, reports to host daemon |
| `port_proxy` | Proxies localhost-bound listeners to `0.0.0.0` for external access |
| `browser` | Sends browser-open requests to the host daemon via the control connection |
| `credential` | Handles git credential protocol (get/store/erase) by forwarding to host |
| `control` | Host daemon communication — sends/receives messages over the control TCP connection |
| `cli` | CLI mode for in-container `cella` commands (when agent binary is symlinked as `cella`) |
| `forward_proxy` | Forward proxy for localhost-bound applications |
| `mitm` | MITM proxy for HTTPS interception and path-level blocking |
| `plugin_sync` | Plugin/extension synchronization for editors |
| `proxy_config` | Network proxy configuration parsing and rule matching |
| `reconnecting_client` | Resilient connection management with retry logic and automatic reconnection |

## Crate Dependencies

**Depends on:** [cella-network](../cella-network), [cella-port](../cella-port)

**Depended on by:** none (standalone binary uploaded into containers)

## Testing

```sh
cargo test -p cella-agent
```

Minimal test surface. The agent is a reliability-focused runtime component — correctness is primarily verified through integration testing with actual containers.

## Development

The agent connects to the host daemon using environment variables set during container creation:
- `CELLA_DAEMON_ADDR` — host daemon address
- `CELLA_DAEMON_TOKEN` — authentication token
- `CELLA_CONTAINER_NAME` — container identifier

Log level is controlled via `CELLA_AGENT_LOG` (or `RUST_LOG`).

The agent protocol must stay in sync with the daemon. Message types are defined in `cella_port::protocol` — changes there affect both this crate and cella-daemon. The `reconnecting_client` module handles connection drops gracefully, which is important because the agent may start before the daemon is ready.
