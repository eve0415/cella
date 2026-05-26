# Network Proxy

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT", "SHOULD", "SHOULD NOT", "RECOMMENDED", "MAY", and "OPTIONAL" in this document are to be interpreted as described in [RFC 2119](https://www.ietf.org/rfc/rfc2119.txt).

## Summary

Cella runs a MITM forward proxy inside dev containers that filters network traffic at the domain and path level. The proxy operates in two modes: denylist (block specific destinations, allow everything else) and allowlist (block everything except listed destinations). Rules use glob patterns for domain and path matching, supporting fine-grained control from broad domain blocks (`*.production.example.com`) to specific path restrictions (`/v1/admin/**`).

When blocking rules are active, the proxy intercepts outbound HTTP/HTTPS connections and evaluates them against the rule set. Domain-only rules block at the CONNECT level without TLS interception. Path-level rules require MITM TLS interception, where the proxy terminates the client's TLS connection, inspects the request path, and either blocks or forwards the request. When a request targets a credential-protected domain, the proxy routes it through the credential tunnel to the host daemon instead of forwarding upstream (see [Credential Protection](credential-protection.md)).

The proxy is only started when blocking rules are active. Without rules, host proxy environment variables are forwarded to the container unchanged.

## Architecture

```mermaid
flowchart LR
    subgraph Container
        process["container process"]
        proxy["cella-agent<br/>MITM proxy<br/>(:18080)"]
    end

    subgraph Host
        daemon["cella daemon"]
        upstream["upstream network"]
    end

    process -->|"HTTP/HTTPS<br/>via proxy env vars"| proxy
    proxy -->|"domain-only rule:<br/>CONNECT-level block"| blocked["BLOCKED<br/>(connection refused)"]
    proxy -->|"path rule:<br/>MITM + evaluate"| decision{verdict}
    decision -->|"allowed"| upstream
    decision -->|"blocked"| blocked
    proxy -->|"credential domain:<br/>tunnel to daemon"| daemon
    daemon -->|"real credential<br/>injected"| upstream
```

### Crate Responsibilities

| Crate | Role |
|---|---|
| `cella-network` | Configuration types (`NetworkConfig`, `ProxyConfig`, `NetworkRule`), glob-based rule matching engine (`RuleMatcher`), CA certificate generation and host CA bundle detection, proxy env var auto-detection, rule merging from multiple sources |
| `cella-config` | `[network]` TOML schema (`Network`, `ProxySettings`, `NetworkRule`), settings deserialization with `deny_unknown_fields`, conversion to `cella-network` types |
| `cella-env` | Proxy environment variable injection into containers, agent proxy config JSON generation (`CELLA_PROXY_CONFIG`), CA bundle injection into container trust stores, credential route injection |

### Decision Flow

When the proxy receives an outbound connection:

1. Extract the target domain (from CONNECT request for HTTPS, from Host header for HTTP)
2. Call `domain_needs_path_inspection(domain)` to determine whether any rule for this domain requires path-level matching
3. If no path inspection is needed, call `evaluate_domain_only(domain)` -- this skips all path-bearing rules and evaluates only domain-level rules. The verdict is applied at the connection level (CONNECT accept/reject).
4. If path inspection is needed, perform MITM TLS interception: terminate the client TLS session with a generated leaf certificate, read the plaintext HTTP request, then call `evaluate(domain, path)` against the full rule set.
5. If the domain matches a credential route, bypass the rule engine and tunnel the request to the daemon (see [Credential Protection](credential-protection.md)).

## Configuration Reference

Network rules and proxy settings are loaded from two sources, merged at container startup:

1. **`cella.toml`** (`.devcontainer/cella.toml`) -- higher precedence on conflicts
2. **`devcontainer.json`** (under `customizations.cella.network`)

See [Configuration](../guides/configuration.md) for the full layered config system.

### cella.toml Schema

```toml
[network]
mode = "denylist"                # "denylist" | "allowlist"

[network.proxy]
enabled = true                   # bool, default: true
http = "http://proxy.corp:3128"  # string?, overrides HTTP_PROXY
https = "http://proxy.corp:3128" # string?, overrides HTTPS_PROXY
no_proxy = "localhost,.internal" # string?, overrides NO_PROXY
ca_cert = "/path/to/ca.pem"     # string?, additional CA cert
proxy_port = 18080               # u16, default: 18080

[[network.rules]]
domain = "*.production.example.com"  # string, required
action = "block"                     # "block" | "allow", required

[[network.rules]]
domain = "api.example.com"
paths = ["/v1/admin/*", "/internal/**"]  # string[], optional
action = "block"
```

### devcontainer.json Schema

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

### Type Reference

| Field | Type | Default | Required | Description |
|---|---|---|---|---|
| `network.mode` | `"denylist" \| "allowlist"` | `"denylist"` | no | Blocking mode; determines default disposition of unmatched traffic |
| `network.proxy.enabled` | `bool` | `true` | no | Enable proxy env var forwarding; `false` disables all proxy injection |
| `network.proxy.http` | `string?` | `null` | no | HTTP proxy URL; overrides `HTTP_PROXY` from host env |
| `network.proxy.https` | `string?` | `null` | no | HTTPS proxy URL; overrides `HTTPS_PROXY` from host env |
| `network.proxy.no_proxy` | `string?` | `null` | no | Comma-separated bypass list; overrides `NO_PROXY` from host env |
| `network.proxy.ca_cert` | `string?` | `null` | no | Path to additional CA certificate file for container trust store injection |
| `network.proxy.proxy_port` | `u16` | `18080` | no | Listen port for the cella-agent forward proxy inside the container |
| `network.rules[].domain` | `string` | -- | yes | Domain glob pattern |
| `network.rules[].paths` | `string[]` | `[]` | no | Path glob patterns; empty means all paths on the matched domain |
| `network.rules[].action` | `"block" \| "allow"` | -- | yes | Whether this rule blocks or allows matching traffic |

Unknown fields in `[network]`, `[network.proxy]`, and `[[network.rules]]` MUST be rejected (`deny_unknown_fields`).

## Modes

### Denylist (Default)

All traffic is allowed unless a rule explicitly blocks it. The mode's default disposition for unmatched traffic is **allow**.

```toml
[network]
mode = "denylist"

[[network.rules]]
domain = "*.production.example.com"
action = "block"
```

Requests to `api.production.example.com` are blocked. All other destinations are allowed.

### Allowlist

All traffic is blocked unless a rule explicitly allows it. The mode's default disposition for unmatched traffic is **block**.

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

Only `registry.npmjs.org` and `github.com` are reachable. All other destinations are blocked.

### Interaction with Credential Protection

When credential protection is active (see [Credential Protection](credential-protection.md)), requests to credential-protected domains bypass the network proxy's rule evaluation entirely. The agent routes these requests through the credential tunnel to the host daemon, which handles upstream HTTPS independently. Network proxy rules do not apply to credential-tunneled requests.

Credential domains operate fail-closed: if the credential tunnel is unavailable, the request receives HTTP 502. Phantom tokens are never forwarded upstream.

## Rule Matching

### Domain Patterns

Domain matching SHALL be **case-insensitive**. The `*` wildcard matches exactly one domain label (the segment between dots). There is no `**` (multi-label) wildcard for domains.

| Pattern | Matches | Does Not Match |
|---|---|---|
| `example.com` | `example.com` | `foo.example.com` |
| `*.example.com` | `foo.example.com`, `bar.example.com` | `foo.bar.example.com`, `example.com` |
| `api.*.internal` | `api.foo.internal`, `api.bar.internal` | `api.foo.bar.internal`, `web.foo.internal` |

Domain patterns are lowercased during compilation. Domains are lowercased during evaluation.

### Path Patterns

Path matching SHALL be **case-sensitive**. Two wildcards are available:

- `*` -- matches exactly one path segment
- `**` -- matches zero or more path segments

| Pattern | Matches | Does Not Match |
|---|---|---|
| `/api/v1` | `/api/v1` | `/api/v2`, `/api/v1/extra` |
| `/api/*` | `/api/users`, `/api/posts` | `/api/users/123`, `/api` |
| `/v1/admin/**` | `/v1/admin`, `/v1/admin/users`, `/v1/admin/users/123/roles` | `/v1/public`, `/v2/admin` |
| `/api/**/delete` | `/api/delete`, `/api/users/delete`, `/api/users/123/delete` | `/api/users/update` |

If a rule has no `paths`, it applies to all paths on the matched domain.

Empty paths are normalized to `/` before evaluation.

### Matching Algorithm

Path patterns with `**` use memoized recursive matching to prevent exponential blowup on pathological patterns like `/**/a/**/b/**/c`. The matcher tracks visited `(pattern_index, segment_index)` pairs and prunes duplicate states.

Domain patterns use strict segment-count matching: the pattern and domain MUST have the same number of labels, with `*` matching any single label.

### Rule Ordering

Rules are evaluated in the order they appear in the merged rule set. The first rule whose domain and path patterns match determines the verdict. If no rule matches, the mode's default disposition applies:

- **Denylist**: allow (no matching deny rule)
- **Allowlist**: block (no matching allow rule)

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

### Two Evaluation Paths

The rule engine exposes two evaluation methods:

| Method | Input | Behavior | Use Case |
|---|---|---|---|
| `evaluate(domain, path)` | Domain + request path | Evaluates all rules (domain-only and path-bearing) | MITM'd HTTPS requests, HTTP requests |
| `evaluate_domain_only(domain)` | Domain only | Skips rules that have path patterns; evaluates only domain-level rules | CONNECT-level decisions where MITM is unavailable |

`evaluate_domain_only` MUST NOT evaluate path-bearing rules against a synthetic path. Path rules are skipped entirely -- they do not contribute to the verdict.

### Rule Verdict

Every evaluation produces a `RuleVerdict`:

| Field | Type | Description |
|---|---|---|
| `allowed` | `bool` | Whether the request is allowed |
| `reason` | `string` | Human-readable explanation (e.g., "blocked by rule: *.prod.internal (block)") |
| `matched_rule` | `string?` | The pattern display string of the matching rule, or `null` if the mode default applied |
| `source` | `string?` | Configuration source of the matching rule (`"cella.toml"` or `"devcontainer.json"`), or `null` if the mode default applied |

### Path Inspection Predicate

`domain_needs_path_inspection(domain)` returns `true` if any rule for the given domain has path patterns. This predicate drives the MITM decision:

- `true`: The proxy MUST perform TLS interception to read the request path before rendering a verdict.
- `false`: Domain-level blocking is sufficient; no MITM is needed. The proxy blocks or allows at the CONNECT level.

## TLS Interception

### CA Lifecycle

When any rule requires path-level inspection (has path patterns), cella generates a self-signed CA certificate for MITM TLS interception. The CA is stored on the host:

- **Certificate**: `~/.cella/proxy/ca.pem`
- **Private key**: `~/.cella/proxy/ca.key`
- **Subject**: `CN=Cella Dev Container CA, O=Cella`

`ensure_ca()` is idempotent: if both files exist, they are loaded and returned. If either is absent, a new CA is generated.

The private key file MUST have permissions `0600` on Unix.

### Certificate Parameter Invariant

`ca_certificate_params()` produces the `CertificateParams` used both when first creating the CA on disk and when re-loading it inside the agent to sign per-domain leaf certificates. The two call sites MUST produce identical params. If the Issuer DN of a leaf certificate does not match the CA's Subject DN, TLS chain validation fails and every MITM'd request receives a connection reset.

### Leaf Certificate Generation

When the proxy performs MITM for a domain, it generates a leaf certificate on-the-fly, signed by the CA, with the target domain as the Subject Alternative Name. The leaf certificate is presented to the container process's TLS client.

### Agent Proxy Config

The CA certificate and key are embedded in the agent's proxy config JSON (`CELLA_PROXY_CONFIG` env var) only when path-level rules are present:

```json
{
  "listen_port": 18080,
  "mode": "denylist",
  "rules": [...],
  "upstream_proxy": "http://proxy.corp:3128",
  "ca_cert_pem": "-----BEGIN CERTIFICATE-----\n...",
  "ca_key_pem": "-----BEGIN PRIVATE KEY-----\n..."
}
```

When no path-level rules exist, `ca_cert_pem` and `ca_key_pem` are `null`. The credential protection system extends this same JSON object with `credential_routes`, `daemon_addr`, `container_nonce`, and `container_name` fields (see [Credential Protection](credential-protection.md)).

### Trust Store Injection

The CA certificate is injected into the container's trust store so that tools inside the container trust MITM'd TLS connections. The injection path depends on the container's OS family, detected via `/etc/os-release`:

| OS Family | Detection | Certificate Path | Update Command |
|---|---|---|---|
| Debian (Ubuntu, Alpine, Mint) | `ID` or `ID_LIKE` contains `debian`, `ubuntu`, `alpine`, `mint` | `/usr/local/share/ca-certificates/cella-host-ca.crt` | `update-ca-certificates` |
| RHEL (Fedora, CentOS, Rocky, openSUSE) | `ID` or `ID_LIKE` contains `rhel`, `fedora`, `centos`, `rocky`, `alma`, `oracle`, `amzn`, `suse` | `/etc/pki/ca-trust/source/anchors/cella-host-ca.crt` | `update-ca-trust` |
| Unknown | No match | Both Debian and RHEL paths | `update-ca-certificates \|\| update-ca-trust` |

The certificate file is uploaded with permissions `0644`. The trust store update command runs as root during post-start injection.

### Host CA Bundle

Cella detects the host's CA trust store and forwards it into containers. Detection uses `rustls-native-certs` (handles macOS Keychain, Windows certificate store, etc.) with fallback to well-known filesystem paths:

| Path | Distribution |
|---|---|
| `/etc/ssl/certs/ca-certificates.crt` | Debian/Ubuntu |
| `/etc/pki/tls/certs/ca-bundle.crt` | RHEL/CentOS |
| `/etc/ssl/ca-bundle.pem` | openSUSE |
| `/etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem` | Fedora |
| `/etc/ssl/cert.pem` | macOS/Alpine |

If `network.proxy.ca_cert` is configured, the specified certificate is appended to the detected host CA bundle before injection.

### MITM Behavior

When the proxy intercepts an HTTPS request via MITM:

1. Accept the CONNECT request from the container process
2. Establish a TLS session with the container process using a dynamically generated leaf certificate for the target domain
3. Read the plaintext HTTP request (method, path, headers)
4. Evaluate the request against the rule set via `evaluate(domain, path)`
5. If blocked, return an HTTP error response over the MITM'd connection
6. If allowed, forward the request to the upstream server (or through the upstream proxy if configured)

Domain-only rules do not require MITM. The proxy blocks at the CONNECT level by refusing the connection before any TLS handshake occurs.

## Host Proxy Integration

### Environment Variable Detection

When `proxy.enabled` is `true` (the default), cella auto-detects proxy settings from the host environment. Config values override environment variables:

| Config Field | Environment Variable (checked in order) |
|---|---|
| `proxy.http` | `HTTP_PROXY`, `http_proxy` |
| `proxy.https` | `HTTPS_PROXY`, `https_proxy` |
| `proxy.no_proxy` | `NO_PROXY`, `no_proxy` |

If a config field is set, it takes precedence over the corresponding environment variable.

### Container Injection

Both uppercase and lowercase variants of proxy env vars are set in the container for maximum tool compatibility (`HTTP_PROXY` and `http_proxy`, `HTTPS_PROXY` and `https_proxy`, `NO_PROXY` and `no_proxy`).

When blocking rules are active, proxy env vars point to the cella-agent's local proxy (`http://127.0.0.1:<proxy_port>`) instead of the upstream proxy. This ensures all traffic routes through the rule engine.

When no blocking rules are active, proxy env vars point directly to the upstream proxy (or the auto-detected host proxy).

### Docker Build Args

Proxy env vars are also injected as Docker build args via `to_build_args()`. Docker automatically recognizes `HTTP_PROXY`, `HTTPS_PROXY`, and `NO_PROXY` as build args without requiring explicit `ARG` declarations in the Dockerfile. Both uppercase and lowercase variants are set.

### NO_PROXY Safety Entries

Safety entries MUST be appended to `NO_PROXY` to prevent proxy loops:

| Entry | Condition |
|---|---|
| `localhost` | Always |
| `127.0.0.1` | Always |
| `::1` | Always |
| `127.0.0.1:<proxy_port>` | When the cella-agent proxy is active |

Deduplication is case-insensitive: existing entries that match a safety entry (ignoring case) are not duplicated.

### Disabling Proxy Forwarding

```toml
[network.proxy]
enabled = false
```

When `enabled` is `false`, no proxy environment variables are injected into the container. `ProxyEnvVars::detect()` returns `None`.

## Rule Merging

When both `cella.toml` and `devcontainer.json` define network configuration, they are merged:

### Mode

`cella.toml` wins if it explicitly sets a mode (the `mode` field is present in the TOML). If `cella.toml` does not set a mode (the field is absent, not just defaulted), the `devcontainer.json` mode is used.

The `Network` settings type tracks `mode` as `Option<NetworkMode>` to distinguish "not set" (`None`) from "explicitly set to denylist" (`Some(Denylist)`).

### Proxy Settings

`cella.toml` wins entirely if its proxy section has any explicit value:

- `http` is `Some`
- `https` is `Some`
- `no_proxy` is `Some`
- `ca_cert` is `Some`
- `enabled` is `false`

If none of these conditions hold, the `devcontainer.json` proxy settings are used.

### Rules

Rules from both sources are combined (union). Each rule is labeled with its source (`"cella.toml"` or `"devcontainer.json"`) for diagnostics.

If the same exact domain string appears in both sources, the `cella.toml` rule takes precedence and the `devcontainer.json` rule is dropped. Domain comparison is exact string match (not case-insensitive or glob-aware).

In the merged rule set, `devcontainer.json` rules (minus duplicates) appear first, followed by all `cella.toml` rules. Combined with first-match-wins evaluation, this means exact-string dedup is the sole mechanism that gives `cella.toml` precedence. For overlapping glob patterns that differ as strings (e.g., `*.example.com` in devcontainer.json vs. `api.example.com` in cella.toml), the devcontainer.json rule is evaluated first and wins if it matches. This is a sharp edge: project-level overrides via `cella.toml` require the exact same domain string as the `devcontainer.json` rule they intend to replace.

## CLI Interface

### `cella network status`

Display the current proxy and blocking configuration for a running container.

```
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

Test whether a URL would be blocked or allowed by the current rules. Evaluates the URL against the merged rule set and displays the verdict, including which rule matched and its source.

```
$ cella network test https://api.production.example.com/v1/data
X BLOCKED: https://api.production.example.com/v1/data
  blocked by rule: *.production.example.com (block)

$ cella network test https://github.com/user/repo
V ALLOWED: https://github.com/user/repo
  allowed (no matching deny rule)
```

### `cella network log`

Tail the proxy's blocked-request log from a running container.

```
$ cella network log

# Raw log inside the container
$ cat /tmp/.cella/proxy.log
$ cella exec -- cat /tmp/.cella/proxy.log
```

### `cella up --no-network-rules`

Start a container without enforcing network blocking rules. This sets `NetworkRulePolicy::Skip`, bypassing all configured rules. Host proxy forwarding still applies if configured.

## Error Handling

### Error Taxonomy

| Condition | Behavior | Diagnostic Code |
|---|---|---|
| CA key pair generation fails | MITM unavailable; path-level rules degrade to domain-only blocking | `cella::network::key_generation` |
| CA certificate generation fails | Same degradation as above | `cella::network::cert_generation` |
| CA file I/O failure (read or write) | MITM unavailable; warning logged | `cella::network::io` |
| Additional CA cert unreadable | Warning logged; injection proceeds without the additional cert | `cella::network::read_cert` |
| Container trust store update fails | Warning logged; tools may reject MITM certificates | Non-fatal (command runs with `\|\| true`) |
| MITM certificate rejected by client | Connection fails; tool-specific TLS error | Expected for cert-pinning clients |
| Domain blocked at CONNECT level | Connection refused; no HTTP response body | Rule verdict logged |
| Path blocked after MITM | HTTP error response over MITM'd connection | Rule verdict logged |
| Credential tunnel unavailable for credential domain | HTTP 502; phantom tokens never forwarded upstream | Fail-closed (see [Credential Protection](credential-protection.md)) |
| Upstream proxy unreachable | Connection failure propagated to container process | Host proxy misconfiguration |

### Degradation Behavior

When CA generation fails (key pair or certificate), path-level blocking degrades to domain-only blocking. Rules with path patterns are effectively ignored for MITM decisions, but domain-level rules continue to function. A warning is logged:

```
Failed to generate CA for MITM: {error}. Path-level blocking will be domain-only.
```

The proxy config JSON sets `ca_cert_pem` and `ca_key_pem` to `null`, and the agent operates without MITM capability.

## Extensions

### Encrypted Client Hello (ECH)

ECH encrypts the TLS SNI field, preventing the proxy from determining the target domain during CONNECT. When ECH is widely deployed, the proxy requires alternative domain resolution:

- **DNS-based routing**: Intercept DNS queries to extract the target domain before the TLS connection begins. Map resolved IPs back to domains for rule evaluation.
- **Config extension**: `network.proxy.ech_strategy = "dns"` selects the resolution strategy.
- **Fallback**: When ECH is detected and no alternative resolution is available, apply the mode's default disposition (allow for denylist, block for allowlist) and log a warning.

### HTTP/2 Inspection

The MITM proxy handles HTTP/1.1 over TLS. HTTP/2 multiplexes multiple request streams over a single TLS connection:

- **Stream-level evaluation**: Demultiplex HTTP/2 frames and evaluate each stream's `:path` pseudo-header against the rule set independently.
- **ALPN negotiation**: During MITM TLS handshake, negotiate HTTP/2 with the container process and upstream server when both support it.
- **Mixed verdicts**: Different streams on the same connection may receive different verdicts. Blocked streams receive RST_STREAM; allowed streams are forwarded.

### Per-container Rules

Different network policies for different containers within a workspace:

- **Config extension**: `network.containers.<name>.mode` and `network.containers.<name>.rules` override the global network config for specific containers.
- **Rule merging**: Per-container rules are merged with global rules, with per-container taking precedence. The merge follows the same union-with-dedup logic as cella.toml/devcontainer.json merging.
- **Implementation**: The agent receives its container-specific merged rule set via `CELLA_PROXY_CONFIG`. No runtime rule resolution is needed.

### QUIC/HTTP/3 Interception

QUIC-based connections bypass the TCP-level CONNECT proxy entirely:

- **UDP interception**: Intercept outbound UDP traffic on port 443 (QUIC) and apply domain-level rules based on the SNI in the QUIC Initial packet.
- **QUIC MITM**: Terminate the QUIC connection, inspect HTTP/3 frames, and forward via a separate QUIC connection to the upstream server.
- **Alt-Svc suppression**: Strip `Alt-Svc` headers from HTTP/1.1 and HTTP/2 responses to prevent clients from upgrading to QUIC, keeping traffic on the interceptable TCP path.

## Limitations

1. **HTTP/1.1 only** -- The MITM proxy handles HTTP/1.1 over TLS. HTTP/2 and HTTP/3 streams are not inspected at the path level. Domain-level blocking still applies to all protocols via CONNECT interception.
2. **Certificate pinning** -- Tools that pin TLS certificates (some `curl` builds, mobile SDKs, security-hardened clients) reject MITM certificates. These tools cannot use path-level network filtering.
3. **Proxy bypass** -- A container process can unset proxy environment variables, use raw sockets, or connect directly to destinations, bypassing the proxy entirely. Network proxy rules are enforced at the proxy level, not the network level.
4. **Single `*` domain wildcard** -- `*` matches exactly one domain label. There is no `**` multi-label wildcard. `*.example.com` does not match `a.b.example.com`; each depth requires an explicit rule.
5. **No runtime rule updates** -- Rules are loaded at container startup and baked into `CELLA_PROXY_CONFIG`. Changing rules requires restarting the container.
6. **Trust store coverage** -- Trust store injection covers Debian-family and RHEL-family distributions. Other distributions or custom base images may require manual CA installation.
7. **ECH** -- Encrypted Client Hello hides the SNI field from the proxy, preventing domain-based routing decisions for ECH-enabled clients. See [Extensions](#encrypted-client-hello-ech) for the mitigation path.
