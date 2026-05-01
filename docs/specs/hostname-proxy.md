# Hostname-Based Port Forwarding

## Overview

cella routes HTTP traffic to dev containers via stable, human-readable hostnames instead of arbitrary port numbers. Each worktree container gets a unique hostname derived from its branch and project name.

## Hostname Scheme

```
{port}.{branch}.{project}.localhost     (non-OrbStack)
{port}.{branch}.{project}.local         (OrbStack via dev.orbstack.domains)
```

| Component | Source | Example |
|-----------|--------|---------|
| `{port}` | Container port number | `3000` |
| `{branch}` | Sanitized git branch name | `feature-auth` |
| `{project}` | devcontainer.json `name` field, fallback to repo dir | `myapp` |

When port is omitted (`{branch}.{project}.localhost`), the proxy routes to the first `forwardPorts` entry or first auto-detected port.

### Branch Name Sanitization

1. Replace `/`, `_`, `.` with `-`
2. Collapse consecutive hyphens
3. Strip leading/trailing hyphens
4. Truncate to 63 characters (DNS label limit)
5. Lowercase

Collision handling: append 4-char SHA-256 suffix when two branches sanitize identically.

## Architecture

```
Browser → Port 80 (hostname proxy) → Route Table → Backend Container
```

The proxy is a hyper-based HTTP reverse proxy in the `cella-proxy` crate. It parses the `Host` header, looks up the route table, and forwards to the backend container via direct IP or agent tunnel.

On OrbStack, the proxy is bypassed. Instead, `dev.orbstack.domains` container labels delegate routing to OrbStack's built-in reverse proxy with automatic TLS.

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
| Container registration | Insert routes for `forwardPorts` |
| Port detected | Insert route for new port |
| Port closed | Remove route |
| Container deregistered | Remove all routes |

## Crate Organization

- `cella-proxy` - hostname parsing, route table, HTTP server, error pages
- `cella-daemon` - lifecycle management, port manager integration
- `cella-backend` - OrbStack label generation
- `cella-protocol` - `project_name`, `branch`, `hostname` fields

## Known Limitations

1. Cookie leakage across branches with `Domain=myapp.localhost`
2. Dev servers may reject unfamiliar `Host` headers (Vite, Next.js, Django, Rails)
3. Safari `.localhost` resolution may be inconsistent
4. Port 80 conflicts fall back to port-based forwarding
5. Non-HTTP traffic (databases, gRPC) uses port-based allocation only
