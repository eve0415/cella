# cella-proxy

> Hostname-based HTTP reverse proxy for dev container port forwarding.

Part of the [cella](../../README.md) workspace.

## Overview

cella-proxy routes incoming HTTP requests to dev container backends based on structured hostnames. A request to `3000.feature-auth.myapp.localhost` is parsed into project, branch, and port components, then resolved against a route table to find the target container.

The proxy supports three backend connectivity modes: direct IP (container network), localhost (existing host-port mapping), and agent tunnel (future). It handles both regular HTTP requests and WebSocket upgrades with bidirectional streaming, sets standard forwarding headers (`X-Forwarded-Host`, `X-Forwarded-For`, `X-Forwarded-Proto`), and strips hop-by-hop headers per RFC 2616.

Hostname parsing accepts both `.localhost` and `.local` (OrbStack) TLDs. Branch names are sanitized into valid DNS labels — slashes, underscores, and dots become hyphens, consecutive hyphens collapse, and labels are truncated to the 63-character DNS limit. A SHA-256-based suffix mechanism handles collisions when distinct branch names sanitize to the same label.

When a route lookup fails or the backend is unreachable, the proxy returns styled HTML error pages listing all registered services with clickable links.

## Architecture

### Key Types

- `RouteTable` — in-memory route table mapping `(project, branch, port)` tuples to backend targets, with container-level bulk operations and default port resolution
- `RouteKey` — lookup key: project name, branch slug, container port
- `BackendTarget` — resolved backend: container ID/name, target port, proxy mode
- `ProxyMode` — how to reach the backend: `Localhost`, `DirectIp`, or `AgentTunnel`
- `SharedRouteTable` — `Arc<RwLock<RouteTable>>` for concurrent access from the server
- `ParsedHostname` — decomposed hostname: optional port, branch slug, project name

### Modules

| Module | Purpose |
|--------|---------|
| `router` | `RouteTable`, `RouteKey`, `BackendTarget`, `ProxyMode` — route storage and lookup with per-container indexing |
| `server` | HTTP server lifecycle, request handling, WebSocket upgrade detection and bidirectional streaming, proxy header injection |
| `hostname` | `Host` header parsing for `.localhost`/`.local` TLDs, DNS label sanitization, collision-safe suffixing, URL construction |
| `error_page` | HTML error page generation for missing routes and unreachable backends, with XSS-safe escaping |

## Crate Dependencies

No cella-* dependencies (foundation crate).

**Depended on by:** [cella-backend](../cella-backend), [cella-daemon](../cella-daemon)

## Testing

```sh
cargo test -p cella-proxy
```

Unit tests cover hostname parsing and sanitization (edge cases for DNS label rules, TLD variants, collision suffixes), route table CRUD operations (insert, lookup, default port resolution, per-container removal, mode updates), and full proxy integration tests (end-to-end HTTP forwarding, WebSocket upgrade with bidirectional echo, streaming without buffering, forwarding header injection, 404/502 error responses).

## Development

The proxy is designed to run inside the cella daemon. Consumers create a `SharedRouteTable`, populate it as containers register, and call `start_proxy_server` with a bind address. The server runs in a spawned tokio task and returns the bound address and join handle.

Route table mutations happen through the `SharedRouteTable` (`Arc<RwLock<RouteTable>>`). The server holds a read lock only for the duration of each request's route lookup.
