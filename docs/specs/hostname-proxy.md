# Hostname-Based Port Forwarding

## Overview

cella routes HTTP traffic to dev containers via stable, human-readable hostnames instead of arbitrary port numbers. Each worktree container gets a unique hostname derived from its branch and project name.

## Hostname Scheme

```
{port}.{branch}.{project}.localhost          (generic Cella HTTP proxy)
{port}.{branch}.{project}.localhost:{proxy}  (when port 80 is unavailable)
{branch}.{project}.local                     (OrbStack default web port)
```

| Component | Source | Example |
|-----------|--------|---------|
| `{port}` | Container port number | `3000` |
| `{branch}` | Sanitized git branch name | `feature-auth` |
| `{project}` | devcontainer.json `name` field, fallback to repo dir | `myapp` |

When port is omitted (`{branch}.{project}.localhost`), the generic proxy routes to the first `forwardPorts` entry or first auto-detected port.

### Branch Name Sanitization

1. Replace `/`, `_`, `.` with `-`
2. Collapse consecutive hyphens
3. Strip leading/trailing hyphens
4. Truncate to 63 characters (DNS label limit)
5. Lowercase

Collision handling: append 4-char SHA-256 suffix when two branches sanitize identically.

## Architecture

```
Browser → Hostname proxy → 127.0.0.1:<host_port> → Existing Cella port proxy → Container
```

The proxy is a hyper-based HTTP reverse proxy in the `cella-proxy` crate. It parses the `Host` header, looks up the route table, and forwards to Cella's already allocated loopback host port. This keeps Docker Desktop, Colima, Linux, and OrbStack behavior on the same routing path.

On OrbStack, `dev.orbstack.domains` and `dev.orbstack.http-port` are used only for the default web port. Additional ports use explicit fallback URLs from `cella ports`; V1 does not promise native per-port OrbStack custom domains.

## Proxy Behavior

- Routes HTTP requests by `Host` header
- Supports WebSocket upgrades (`Connection: Upgrade` + `Upgrade: websocket`)
- Sets `X-Forwarded-Host`, `X-Forwarded-Proto`, `X-Forwarded-For` headers
- Strips hop-by-hop headers (except during WebSocket upgrades)
- Returns friendly HTML error pages for unmatched routes and unreachable backends

## Route Table

Routes are managed via `PortManager` events:

| Event | Action |
|-------|--------|
| Container registration | Preload `forwardPorts`, start host-port proxies, insert routes |
| Port detected | Insert or update route using the allocated host port |
| Port closed | Remove route |
| Container deregistered | Remove all routes |

## Crate Organization

- `cella-proxy` - hostname parsing, route table, HTTP server, error pages
- `cella-daemon` - lifecycle management, port manager integration
- `cella-backend` - OrbStack label generation
- `cella-protocol` - `project_name`, `branch`, `ForwardedPortDetail.hostname`, `HostnameProxyStatus`

## Known Limitations

1. HTTP only for the generic Cella hostname proxy; no CA install or HTTPS termination.
2. Loopback only; no LAN exposure or arbitrary TLD support.
3. Cookie leakage can happen across branches with `Domain=myapp.localhost`.
4. Dev servers may reject unfamiliar `Host` headers (Vite, Next.js, Django, Rails).
5. Safari `.localhost` resolution may be inconsistent.
6. Non-HTTP traffic (databases, raw TCP, UDP) uses port-based allocation only.
7. OrbStack native custom domains are limited to the default web port in V1.
