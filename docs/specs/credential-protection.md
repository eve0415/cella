# Credential Protection

## Overview

Credential protection prevents dev container processes from reading real API keys and tokens. Instead of forwarding credentials as environment variables, cella injects opaque **phantom tokens** that are meaningless outside the daemon. When a container process makes an API request, the in-container agent intercepts requests to known credential domains and tunnels them to the host daemon, which replaces the phantom token with the real credential and makes the upstream HTTPS request on behalf of the container.

Enable with `credentials.protect = true` in `cella.toml`.

## Threat Model

### In Scope

- **Malicious code in containers** reading env vars, files, or network traffic to exfiltrate credentials (supply chain attacks via compromised dependencies, backdoored VS Code extensions, malicious devcontainer features)
- **Container escapes via Docker API** abusing mounted Docker sockets or API access to read host-side secrets
- **Credential harvesting from CI/CD** where untrusted code runs in ephemeral containers with access to production API keys

Real-world incidents motivating this design:

- Nx Console (May 2026) — VS Code extension update included credential-harvesting code
- TanStack npm packages — supply chain compromise targeting API keys in CI environments
- axios RAT — backdoor in popular HTTP library exfiltrating environment variables
- Shai-Hulud — containerized crypto-miner that escalated through Docker socket access

### Out of Scope

- Compromised host OS or daemon process (the daemon is the trust root)
- Kernel-level side channels or memory inspection from the host
- Network-level attacks between daemon and upstream APIs (relies on TLS)

### Trust Boundaries

| Boundary | Trust Level |
|---|---|
| Host daemon | Fully trusted — holds real credentials, performs resolution |
| Host filesystem | Trusted — stores state file with 0600 perms |
| Agent (in-container) | Semi-trusted — bridges MITM proxy to daemon tunnel, never sees real credentials |
| Container processes | Untrusted — only see phantom tokens |
| Upstream APIs | External — TLS-protected, daemon is the HTTPS client |

## Goals and Non-Goals

| Goals (v1) | Non-Goals (v1) |
|---|---|
| Phantom token substitution for all built-in providers | Multi-account profiles (config plumbed, not wired) |
| Daemon-side credential resolution (never cached) | `.env` file protection |
| Per-provider enable/disable toggles | Dynamic provider plugin loading |
| Custom provider registration via TOML | Structured audit log for credential usage |
| GitHub credential forwarding via `gh auth token` | HTTP/3 support for upstream requests |
| Streaming response proxying | Credential rotation / TTL |
| State persistence across daemon restarts | Browser-based OAuth flows |
| Docker Engine version checks in `cella doctor` | Per-container credential scoping beyond provider-level |

## Architecture

```
Container                           Host
┌──────────────────────────────┐    ┌──────────────────────────────────────────┐
│                              │    │                                          │
│  process ──► HTTP request    │    │                                          │
│      │  (phantom token in    │    │                                          │
│      │   auth header)        │    │                                          │
│      ▼                       │    │                                          │
│  agent MITM proxy            │    │                                          │
│      │  matches credential   │    │                                          │
│      │  domain route         │    │                                          │
│      ▼                       │    │                                          │
│  credential tunnel ─────TCP──┼───►│  daemon control port                     │
│  (CredentialProxyHandshake)  │    │      │                                   │
│                              │    │      ▼                                   │
│                              │    │  credential_proxy handler                │
│                              │    │      │  1. validate phantom token        │
│                              │    │      │  2. check provider match          │
│                              │    │      │  3. verify domain registration    │
│                              │    │      │  4. resolve real credential       │
│                              │    │      │  5. make upstream HTTPS request   │
│                              │    │      ▼                                   │
│  ◄── streamed response ─────┼────│  upstream API (api.anthropic.com, etc.)  │
│                              │    │                                          │
└──────────────────────────────┘    └──────────────────────────────────────────┘
```

### Crate Responsibilities

| Crate | Role |
|---|---|
| `cella-config` | `[credentials]` TOML schema, `AiCredentials` per-provider toggles |
| `cella-env` | Built-in provider registry (`CREDENTIAL_PROVIDERS`), proxy config injection, `CredentialRouteConfig` |
| `cella-protocol` | `CredentialProxyHandshake`, `PhantomTokenEntry`, `RegisterPhantomTokens`/`GetPhantomTokens` management messages |
| `cella-orchestrator` | Phantom token generation, daemon registration, credential route building, container label injection |
| `cella-daemon` | Phantom registry (persistence + lookup), credential proxy handler (validation + upstream request), credential resolver |
| `cella-agent` | MITM proxy credential domain routing, credential tunnel establishment |
| `cella-cli` | Wires credential protection into `up`/`exec` flows |
| `cella-doctor` | Docker Engine version checks for security-relevant thresholds |

### Design Rationale

**Daemon-side injection** — The daemon holds real credentials and makes upstream requests. No credential material ever enters the container, even transiently. This eliminates the entire class of env-var/file/memory-scanning attacks.

**Structured envelope** — The credential tunnel uses a JSON header line + length-prefixed body chunks rather than raw HTTP proxying. This gives the daemon full control over header injection and avoids the complexity of a general-purpose HTTPS MITM on the host side.

**Fail-closed** — When credential protection is active, requests to credential domains that cannot be tunneled receive a 502. There is no fallback to passing the phantom token through to the upstream API.

## Built-in Providers

12 providers ship built-in. Custom providers can override any built-in by matching the `id`.

| ID | Env Var | Domains | Header | Prefix |
|---|---|---|---|---|
| `github` | `GH_TOKEN` | `github.com`, `api.github.com` | `Authorization` | `token ` |
| `anthropic` | `ANTHROPIC_API_KEY` | `api.anthropic.com` | `x-api-key` | _(none)_ |
| `openai` | `OPENAI_API_KEY` | `api.openai.com` | `Authorization` | `Bearer ` |
| `gemini` | `GEMINI_API_KEY` | `generativelanguage.googleapis.com` | `x-goog-api-key` | _(none)_ |
| `groq` | `GROQ_API_KEY` | `api.groq.com` | `Authorization` | `Bearer ` |
| `mistral` | `MISTRAL_API_KEY` | `api.mistral.ai` | `Authorization` | `Bearer ` |
| `deepseek` | `DEEPSEEK_API_KEY` | `api.deepseek.com` | `Authorization` | `Bearer ` |
| `xai` | `XAI_API_KEY` | `api.x.ai` | `Authorization` | `Bearer ` |
| `fireworks` | `FIREWORKS_API_KEY` | `api.fireworks.ai` | `Authorization` | `Bearer ` |
| `together` | `TOGETHER_API_KEY` | `api.together.xyz` | `Authorization` | `Bearer ` |
| `perplexity` | `PERPLEXITY_API_KEY` | `api.perplexity.ai` | `Authorization` | `Bearer ` |
| `cohere` | `COHERE_API_KEY` | `api.cohere.com` | `Authorization` | `Bearer ` |

The GitHub provider is special-cased: it uses `gh auth token -h <hostname>` for resolution instead of reading an env var directly. This supports GitHub Enterprise multi-host setups and respects the user's `gh` CLI authentication state. The GitHub provider is gated on `credentials.gh` (default: `true`) and `gh auth status` succeeding.

AI providers are gated on: `credentials.ai.enabled` (default: `true`), the per-provider toggle (default: `true`), and the host env var being set and non-empty.

## Custom Provider Configuration

Add custom providers in `cella.toml` with `[[credentials.providers]]`:

```toml
[[credentials.providers]]
name = "internal-api"
env = "INTERNAL_API_KEY"
domain = "api.internal.corp"
header = "Authorization"
prefix = "Bearer "
```

| Field | Type | Required | Default | Description |
|---|---|---|---|---|
| `name` | `string` | yes | — | Short identifier; if it matches a built-in ID, overrides that provider |
| `env` | `string` | yes | — | Host environment variable holding the real credential |
| `domain` | `string` | yes | — | Target domain this provider protects |
| `header` | `string` | yes | — | HTTP header name for credential injection |
| `prefix` | `string` | no | `""` | Value prefix prepended to the credential (e.g. `"Bearer "`) |

**Override semantics:** A custom provider with the same `name` as a built-in completely replaces the built-in. The built-in's domains, header, and env var are discarded.

Unknown fields in `[[credentials.providers]]` are rejected (`deny_unknown_fields`).

## Configuration Reference

Full `[credentials]` section in `cella.toml`:

```toml
[credentials]
# Forward gh CLI credentials (default: true)
gh = true

# Enable phantom token protection (default: false)
protect = true

# Credential profile name (reserved for future multi-account support)
# profile = "work"

# AI provider forwarding
[credentials.ai]
enabled = true       # Global toggle (default: true)
openai = false       # Disable specific providers
groq = false

# Custom providers
[[credentials.providers]]
name = "internal-api"
env = "INTERNAL_API_KEY"
domain = "api.internal.corp"
header = "x-api-key"

[[credentials.providers]]
name = "anthropic"        # Overrides the built-in anthropic provider
env = "MY_ANTHROPIC_KEY"
domain = "custom-anthropic.corp"
header = "Authorization"
prefix = "Bearer "
```

| Field | Type | Default | Description |
|---|---|---|---|
| `gh` | `bool` | `true` | Forward GitHub credentials via `gh auth token` |
| `protect` | `bool` | `false` | Enable phantom token credential protection |
| `profile` | `string?` | `null` | _(Reserved)_ Credential profile name for multi-account scoping |
| `ai.enabled` | `bool` | `true` | Global toggle for AI provider key forwarding |
| `ai.<provider_id>` | `bool` | `true` | Per-provider override (e.g. `ai.openai = false`) |
| `providers` | `array` | `[]` | Custom provider definitions (see above) |

## Phantom Token Lifecycle

### 1. Generation

During `cella up`, `generate_phantom_tokens()` iterates all merged providers (built-in + custom). For each provider where the host credential is available, it generates a UUID-based phantom token:

```
pt-550e8400-e29b-41d4-a716-446655440000
```

The `pt-` prefix makes phantom tokens visually distinguishable from real credentials.

### 2. Registration

Phantom tokens are registered with the daemon via the management socket (`RegisterPhantomTokens`). Each entry includes the full provider metadata: `provider_id`, `phantom_token`, `env_var`, `domains`, `header`, `prefix`. If registration fails, the orchestrator logs a warning and continues — credential protection is best-effort at registration time but fail-closed at request time.

### 3. Container Injection

Credential routes are injected into the agent's proxy config (`CELLA_PROXY_CONFIG` JSON). The config includes:
- `credential_routes`: array of `{domain, provider_id}` objects
- `daemon_addr`: host daemon TCP address (e.g. `127.0.0.1:9876`)
- `daemon_token`: auth token for tunnel connections
- `container_name`: container identifier for registry lookups

Container labels `dev.cella.credential_protect=true` and `dev.cella.container_name=<name>` are added for mode signaling.

### 4. Exec-time Injection

On `cella exec` or `cella shell`, the CLI queries the daemon (`GetPhantomTokens`) and injects phantom tokens as environment variables:

```
ANTHROPIC_API_KEY=pt-550e8400-e29b-41d4-a716-446655440000
```

The container process sees what looks like an API key in its environment but the value is an opaque phantom token.

### 5. Request-time Resolution

When a container process makes an HTTP request to a credential domain (e.g. `api.anthropic.com`), the agent's MITM proxy intercepts it, opens a credential tunnel to the daemon, and the daemon:

1. Reads the `CredentialProxyHandshake` to identify the container, domain, and provider
2. Parses the `HttpRequestEnvelope` (method, URI, headers, body length)
3. Extracts the phantom token from the auth header
4. Validates: phantom token exists in registry, provider matches, domain is registered
5. Resolves the real credential (env var read or `gh auth token`)
6. Strips the phantom token header, injects the real credential header
7. Makes the upstream HTTPS request (no redirect following)
8. Streams the response back through the tunnel

### 6. Persistence

The phantom registry is persisted to `~/.cella/phantom-registry.state` on every registration and removal. Format:

```json
{
  "schema_version": 1,
  "daemon_pid": 12345,
  "written_at_unix_sec": 1748131200,
  "containers": {
    "cella-myapp-main": {
      "tokens": [
        {
          "provider_id": "anthropic",
          "phantom_token": "pt-550e8400-e29b-41d4-a716-446655440000",
          "env_var": "ANTHROPIC_API_KEY",
          "domains": ["api.anthropic.com"],
          "header": "x-api-key",
          "prefix": ""
        }
      ]
    }
  }
}
```

Written atomically via tmp + rename. File permissions are `0600` on Unix. On daemon startup, `reclaim_from_state_file()` restores the registry from this file, enabling phantom tokens to survive daemon restarts.

### 7. Cleanup

When a container is deregistered, `remove_container()` clears all phantom tokens for that container and persists the updated state. Re-registration (e.g. `cella up` on an existing container) replaces all tokens atomically.

## Protocol Additions

### TCP Handshake Discrimination

Three handshake types share the daemon's control TCP port. The daemon discriminates by attempting JSON deserialization in order:

| Handshake | Discriminating Fields | Purpose |
|---|---|---|
| `CredentialProxyHandshake` | has `provider_id` + `request_id` | Credential tunnel connection |
| `TunnelHandshake` | has `connection_id`, no `provider_id` | Reverse tunnel for port forwarding |
| `AgentHello` | has `protocol_version`, neither of the above | Agent control connection |

### CredentialProxyHandshake

Sent by the agent as the first message on a credential tunnel TCP connection.

| Field | Type | Description |
|---|---|---|
| `auth_token` | `string` | Auth token for validating the connection |
| `container_name` | `string` | Container name for registry lookups |
| `request_id` | `string` | Unique request ID for logging |
| `domain` | `string` | Target API domain (e.g. `api.anthropic.com`) |
| `provider_id` | `string` | Provider ID (e.g. `anthropic`) |

```json
{"auth_token":"abc123","container_name":"cella-myapp-main","request_id":"cred-1","domain":"api.anthropic.com","provider_id":"anthropic"}
```

### PhantomTokenEntry

Used in `RegisterPhantomTokens` and the state file.

| Field | Type | Default | Description |
|---|---|---|---|
| `provider_id` | `string` | — | Provider identifier |
| `phantom_token` | `string` | — | Opaque replacement token (`pt-<uuid>`) |
| `env_var` | `string` | — | Host env var name |
| `domains` | `string[]` | — | API domains this token is valid for |
| `header` | `string` | `""` | HTTP header name for injection |
| `prefix` | `string` | `""` | Header value prefix |

### Management Messages

**`register_phantom_tokens`** — Register phantom tokens for a container.

| Field | Type | Description |
|---|---|---|
| `container_name` | `string` | Container name |
| `tokens` | `PhantomTokenEntry[]` | Phantom token entries to register |

```json
{"type":"register_phantom_tokens","container_name":"cella-myapp-main","tokens":[{"provider_id":"anthropic","phantom_token":"pt-abc","env_var":"ANTHROPIC_API_KEY","domains":["api.anthropic.com"],"header":"x-api-key","prefix":""}]}
```

Response: `phantom_tokens_registered`

| Field | Type | Description |
|---|---|---|
| `container_name` | `string` | Container name confirmed |

**`get_phantom_tokens`** — Retrieve phantom token env var mappings for exec-time injection.

| Field | Type | Description |
|---|---|---|
| `container_name` | `string` | Container name |

Response: `phantom_token_values`

| Field | Type | Description |
|---|---|---|
| `tokens` | `map<string, string>` | Env var name → phantom token value |

```json
{"type":"phantom_token_values","tokens":{"ANTHROPIC_API_KEY":"pt-abc-123","OPENAI_API_KEY":"pt-def-456"}}
```

### HTTP Envelope Wire Format

The credential tunnel uses a custom binary-framed protocol over TCP (not raw HTTP):

**Request (agent → daemon):**
1. JSON line: `HttpRequestEnvelope` — `{method, uri, headers, body_len}` terminated by `\n`
2. Body: `body_len` raw bytes (0 bytes if `body_len == 0`)

**Response (daemon → agent):**
1. JSON line: `HttpResponseEnvelope` — `{status, headers}` terminated by `\n`
2. Body chunks: sequence of `[4-byte BE u32 length][data bytes]`, terminated by `0u32` (4 zero bytes)

| Envelope Field | Type | Description |
|---|---|---|
| `method` | `string` | HTTP method |
| `uri` | `string` | Request path + query (e.g. `/v1/messages`) |
| `headers` | `[string, string][]` | Header key-value pairs |
| `body_len` | `u32` | Request body length in bytes (max 256 MB) |
| `status` | `u16` | HTTP response status code |

## Security

### Phantom Token Validation

Every credential proxy request passes through a four-step validation:

1. **Token lookup** — The phantom token is extracted from the auth header (stripping `Bearer `, `token `, or raw). Looked up in the registry by `(container_name, phantom_token)` → `provider_id`.
2. **Provider mismatch check** — The resolved `provider_id` must match the `provider_id` in the `CredentialProxyHandshake`. Prevents a container from using one provider's phantom token to access another provider's credential.
3. **Domain verification** — The `domain` in the handshake must be in the provider's registered domain list. Prevents tunneling requests to arbitrary domains.
4. **Credential resolution** — The real credential is resolved live (env var read or `gh auth token`). If the credential is unavailable, the request fails.

Any validation failure returns HTTP 403 with an empty body.

### Credential Isolation

- **Never cached** — `resolve_credential()` reads the host env var or runs `gh auth token` on every request. No credential material is stored in memory beyond the lifetime of a single request handler.
- **Host-only resolution** — Real credentials only exist in the daemon process on the host. They are never sent to the container, written to the container filesystem, or passed through environment variables.
- **Per-request GitHub tokens** — `gh auth token -h <hostname>` is invoked per-request, respecting token rotation and multi-host GHE configurations.

### MITM Requirement

Credential domains are routed through the agent's MITM proxy. If the agent cannot establish a credential tunnel to the daemon (e.g. daemon unreachable, auth failure), the request fails with HTTP 502. There is no fallback to passing requests through directly — this is fail-closed by design.

### Request Hardening

- **No redirect following** — The upstream HTTP client is configured with `reqwest::redirect::Policy::none()`. Prevents open-redirect attacks that could leak credentials to attacker-controlled domains.
- **Body size limit** — Request body is capped at 256 MB (`MAX_BODY_LEN`). Bodies exceeding this limit are rejected at the protocol level before any upstream request is made.
- **Header stripping** — The phantom token auth header and `Host` header are stripped from the upstream request. The real credential header is injected by the daemon, and `Host` is set by reqwest from the URL.

### Container Labels

Credential-protected containers are labeled for mode signaling:

| Label | Value | Purpose |
|---|---|---|
| `dev.cella.credential_protect` | `"true"` | Indicates phantom token protection is active |
| `dev.cella.container_name` | Container name | Links the container to its phantom registry entry |

## Failure Modes

| Condition | Behavior | Status | Rationale |
|---|---|---|---|
| Phantom token not found in auth header | Reject | 403 | No credential to resolve |
| Phantom token not in registry | Reject | 403 | Unknown or expired token |
| Provider ID mismatch (handshake vs registry) | Reject | 403 | Prevents cross-provider credential access |
| Domain not in provider's registered domains | Reject | 403 | Prevents credential use on unregistered domains |
| Real credential unavailable (env var unset/empty) | Reject | 403 | Cannot resolve — fail closed |
| `gh auth token` fails or returns empty | Reject | 403 | GitHub CLI not authenticated |
| Invalid request envelope (malformed JSON) | Error | 502 | Protocol violation |
| Request body exceeds 256 MB | Error | 502 | Body size limit exceeded |
| Socket I/O failure | Error | 502 | Connection lost |
| Upstream request failure (DNS, TLS, timeout) | Error | 502 | Cannot reach upstream API |
| Daemon registration failure | Log warning, continue | — | Best-effort; fail-closed at request time |

## Container Hardening

`cella doctor` checks Docker Engine version for security-relevant thresholds:

| Version | Check | Detail |
|---|---|---|
| < 29.0 | Warning | Default seccomp profile does not block `AF_ALG` (CVE-2026-31431 mitigation). Credential protection benefits from 29+ hardening. |
| 29.0 – 29.3.0 | Warning | CVE-2026-34040: AuthZ bypass via oversized requests. Upgrade to 29.3.1+. |
| >= 29.3.1 | Pass | All known security-relevant patches applied. |

## Limitations (v1)

1. `credentials.protect` defaults to `false` — opt-in only.
2. `credentials.profile` is plumbed in config but has no consumer. Multi-account scoping is deferred.
3. Custom providers support a single domain per entry. Multi-domain custom providers require multiple `[[credentials.providers]]` entries with the same `name`.
4. Credential resolution is synchronous per-request — no connection pooling to upstream APIs across credential tunnel connections.
5. No audit log for credential usage beyond standard tracing (`CRED_PROXY` info lines).
6. State file is not encrypted — relies on filesystem permissions (0600) for protection.
7. Phantom tokens are static for the lifetime of a container. No rotation or TTL.
8. The credential tunnel does not support WebSocket upgrade or HTTP/2 — it is a request/response protocol.
9. No support for credentials stored in keychains, vaults, or secret managers (only env vars and `gh auth token`).

## Future Work

- **Multi-account profiles** — `credentials.profile` scoping to select different credential sets per project
- **`.env` file protection** — intercept `.env` file reads and inject phantom tokens
- **Dynamic provider plugins** — load provider definitions from external files or registries
- **Structured audit log** — machine-readable log of credential proxy requests (provider, domain, status, timing)
- **HTTP/2 and HTTP/3** — upgrade the credential tunnel transport
- **Credential rotation** — TTL-based phantom token rotation with re-registration
- **Vault/keychain integration** — resolve credentials from HashiCorp Vault, 1Password, system keychains
- **Per-container credential scoping** — different credential sets per container within a workspace
- **Browser-based OAuth flows** — support providers that require interactive authentication
