# Network Proxy

Cella runs a transparent MITM proxy inside dev containers that can block or allow network traffic at the domain and path level. Rules are glob-based, support HTTPS interception for path-level decisions, and merge from two configuration sources. When no blocking rules are configured, host proxy settings are forwarded through to the container unchanged.

## Configuration

Network rules and proxy settings come from two sources:

1. **`cella.toml`** (project-level, `.devcontainer/cella.toml`) -- takes precedence on conflict
2. **`devcontainer.json`** (under `customizations.cella.network`)

### cella.toml

```toml
[network]
mode = "denylist"

[network.proxy]
enabled = true
http = "http://proxy.corp:3128"
https = "http://proxy.corp:3128"
no_proxy = "localhost,.internal"
proxy_port = 18080

[[network.rules]]
domain = "*.production.example.com"
action = "block"

[[network.rules]]
domain = "api.example.com"
paths = ["/v1/admin/*", "/internal/**"]
action = "block"

[[network.rules]]
domain = "registry.npmjs.org"
action = "allow"
```

### devcontainer.json

```jsonc
{
  "customizations": {
    "cella": {
      "network": {
        "mode": "denylist",
        "rules": [
          {
            "domain": "*.production.example.com",
            "action": "block"
          },
          {
            "domain": "api.example.com",
            "paths": ["/v1/admin/*", "/internal/**"],
            "action": "block"
          }
        ]
      }
    }
  }
}
```

### Merge behavior

When both sources define network config, they are merged as follows:

- **Mode**: `cella.toml` wins if it explicitly sets a mode; otherwise `devcontainer.json`'s mode is used.
- **Proxy settings**: `cella.toml` wins entirely if it sets any explicit proxy values (http, https, no_proxy, ca_cert, or enabled=false).
- **Rules**: union of both sources. If the same exact domain string appears in both, the `cella.toml` rule takes precedence and the `devcontainer.json` rule is dropped.

## Modes

### Denylist (default)

All traffic is allowed unless a rule explicitly blocks it. Use this to block specific domains or paths while leaving everything else open.

```toml
[network]
mode = "denylist"

[[network.rules]]
domain = "*.production.example.com"
action = "block"
```

Result: requests to `api.production.example.com` are blocked. Everything else is allowed.

### Allowlist

All traffic is blocked unless a rule explicitly allows it. Use this for strict environments where containers should only reach approved destinations.

```toml
[network]
mode = "allowlist"

[[network.rules]]
domain = "registry.npmjs.org"
action = "allow"

[[network.rules]]
domain = "github.com"
action = "allow"
```

Result: only `registry.npmjs.org` and `github.com` are reachable. Everything else is blocked.

## Rules

Rules match against a domain and optionally against URL paths. The first matching rule wins.

### Domain patterns

Domain matching is **case-insensitive**. The `*` wildcard matches exactly one domain label (segment between dots).

| Pattern | Matches | Does not match |
|---------|---------|----------------|
| `example.com` | `example.com` | `foo.example.com` |
| `*.example.com` | `foo.example.com`, `bar.example.com` | `foo.bar.example.com`, `example.com` |
| `api.*.internal` | `api.foo.internal`, `api.bar.internal` | `api.foo.bar.internal`, `web.foo.internal` |

There is no `**` (multi-segment) wildcard for domains. Each `*` matches exactly one label.

### Path patterns

Path matching is **case-sensitive**. Two wildcards are available:

- `*` -- matches exactly one path segment
- `**` -- matches zero or more path segments

| Pattern | Matches | Does not match |
|---------|---------|----------------|
| `/api/v1` | `/api/v1` | `/api/v2`, `/api/v1/extra` |
| `/api/*` | `/api/users`, `/api/posts` | `/api/users/123`, `/api` |
| `/v1/admin/**` | `/v1/admin`, `/v1/admin/users`, `/v1/admin/users/123/roles` | `/v1/public`, `/v2/admin` |
| `/api/**/delete` | `/api/delete`, `/api/users/delete`, `/api/users/123/delete` | `/api/users/update` |

If a rule has no `paths`, it applies to all paths on the matched domain.

### Path inspection and MITM

When a rule specifies path patterns, cella must perform TLS interception (MITM) for HTTPS requests to that domain so it can read the request path. Domain-only rules do not require MITM -- the proxy blocks at the connection level.

### Rule ordering

Rules are evaluated in order. The first rule whose domain and path patterns match determines the verdict. If no rule matches, the mode's default applies (allow for denylist, block for allowlist).

```toml
# Allow /public/** on api.example.com, block everything else on that domain
[[network.rules]]
domain = "api.example.com"
paths = ["/public/**"]
action = "allow"

[[network.rules]]
domain = "api.example.com"
action = "block"
```

## CA Certificates

When blocking rules require HTTPS interception, cella auto-generates a CA certificate and private key:

- Certificate: `~/.cella/proxy/ca.pem`
- Private key: `~/.cella/proxy/ca.key`

The CA is created on first use and reused across containers. The private key has restricted permissions (0600 on Unix). The certificate's subject is `CN=Cella Dev Container CA, O=Cella`.

The CA certificate is injected into containers so that tools inside the container trust the proxy's intercepted TLS connections.

### Additional CA certificates

To inject a custom CA certificate (e.g., a corporate root CA) into containers:

```toml
[network.proxy]
ca_cert = "/path/to/corporate-ca.pem"
```

### Host CA bundle

Cella detects the host's CA trust store and can forward it into containers. Detection uses `rustls-native-certs` (handles macOS Keychain, etc.) with fallback to well-known paths:

- `/etc/ssl/certs/ca-certificates.crt` (Debian/Ubuntu)
- `/etc/pki/tls/certs/ca-bundle.crt` (RHEL/CentOS)
- `/etc/ssl/ca-bundle.pem` (openSUSE)
- `/etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem` (Fedora)
- `/etc/ssl/cert.pem` (macOS/Alpine)

## Host Proxy Forwarding

When `proxy.enabled` is `true` (the default), cella auto-detects `HTTP_PROXY`, `HTTPS_PROXY`, and `NO_PROXY` from the host environment and forwards them into containers. Both uppercase and lowercase variants are set for maximum tool compatibility.

Config values override environment variables:

```toml
[network.proxy]
http = "http://proxy.corp:3128"    # overrides HTTP_PROXY from env
https = "http://proxy.corp:3128"   # overrides HTTPS_PROXY from env
no_proxy = "localhost,.internal"   # overrides NO_PROXY from env
```

Safety entries (`localhost`, `127.0.0.1`, `::1`) are always appended to `NO_PROXY` to prevent proxy loops. When the cella-agent proxy is active, its address (`127.0.0.1:<proxy_port>`) is also added.

To disable proxy forwarding entirely:

```toml
[network.proxy]
enabled = false
```

## CLI Commands

### `cella network status`

Show the current proxy and blocking configuration.

```sh
$ cella network status
Proxy: active (localhost:18080)
Upstream HTTP: http://proxy.corp:3128
Upstream HTTPS: http://proxy.corp:3128
CA: auto-generated (~/.cella/proxy/ca.pem)
Mode: denylist (3 rules)
  block: *.production.example.com
  block: api.example.com [/v1/admin/*, /internal/**]
  allow: registry.npmjs.org
```

### `cella network test <url>`

Test whether a URL would be blocked or allowed by the current rules.

```sh
$ cella network test https://api.production.example.com/v1/data
X BLOCKED: https://api.production.example.com/v1/data
  blocked by rule: *.production.example.com (block)

$ cella network test https://github.com/user/repo
V ALLOWED: https://github.com/user/repo
  allowed (no matching deny rule)
```

### `cella network log`

View the proxy's blocked-request log from a running container.

```sh
# Inside the container
$ cat /tmp/.cella/proxy.log

# From the host
$ cella exec -- cat /tmp/.cella/proxy.log
```

## Disabling

To start a container without any network blocking rules:

```sh
$ cella up --no-network-rules
```

This bypasses all configured rules. Host proxy forwarding still applies if configured.

## Proxy Port

The in-container proxy listens on port 18080 by default. To change it:

```toml
[network.proxy]
proxy_port = 19090
```

The proxy is only started when blocking rules are active. Without rules, proxy environment variables point directly to the upstream proxy (or are unset).
