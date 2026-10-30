# Network Proxy

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT", "SHOULD", "SHOULD NOT", "RECOMMENDED", "MAY", and "OPTIONAL" in this document are to be interpreted as described in [RFC 2119](https://www.ietf.org/rfc/rfc2119.txt).

## Summary

Cella runs a MITM forward proxy inside dev containers that filters network traffic at the domain and path level. The proxy operates in two modes: denylist (block specific destinations, allow everything else) and allowlist (block everything except listed destinations). Rules use glob patterns for domain and path matching, supporting fine-grained control from broad domain blocks (`*.production.example.com`) to specific path restrictions (`/v1/admin/**`). Besides blocking and allowing, a rule can answer matching requests with a configured response sequence (`respond`) or add headers to the upstream's responses (`response_headers` on an `allow` rule).

When the proxy is active, it intercepts outbound HTTP/HTTPS connections and evaluates them against the rule set. Domain-only `block` and `allow` rules decide at the CONNECT level without TLS interception. Rules with path patterns, `respond` rules, and rules with `response_headers` require MITM TLS interception, where the proxy terminates the client's TLS connection, reads each request, and blocks it, answers it with the configured response, or forwards it. Plain HTTP requests are evaluated one by one, including every request on a keep-alive connection. When a request targets a credential-protected domain, the proxy routes it through the credential tunnel to the host daemon instead of forwarding upstream (see [Credential Protection](credential-protection.md)).

The proxy is started when any network rule is configured or credential protection is on, on a backend that provisions the cella agent. Otherwise, host proxy environment variables are forwarded to the container unchanged.

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
    proxy -->|"domain-only rule:<br/>CONNECT-level block"| blocked["BLOCKED<br/>(403, connection closed)"]
    proxy -->|"path, respond or<br/>response-header rule:<br/>MITM + evaluate"| decision{verdict}
    decision -->|"allowed"| upstream
    decision -->|"blocked"| blocked
    decision -->|"respond"| mock["configured response<br/>(no upstream connection)"]
    proxy -->|"credential domain:<br/>tunnel to daemon"| daemon
    daemon -->|"real credential<br/>injected"| upstream
```

### Crate Responsibilities

| Crate | Role |
|---|---|
| `cella-network` | Configuration types (`NetworkConfig`, `ProxyConfig`, `NetworkRule`, `ResponseSpec`), rule validation (`NetworkConfig::validate`), glob-based rule matching engine (`RuleMatcher`), CA certificate generation and host CA bundle detection, proxy env var auto-detection |
| `cella-config` | `[network]` TOML schema (`Network`, `ProxySettings`, `NetworkRule`, `ResponseSpec`), settings deserialization with `deny_unknown_fields`, per-layer `body_file` resolution, rejection of `respond` fields in `customizations.cella`, conversion to `cella-network` types (`to_network_config`), which reads each `body_file` |
| `cella-env` | Proxy environment variable injection into containers, agent proxy config JSON generation (`CELLA_PROXY_CONFIG`), CA bundle injection into container trust stores, credential route injection |

### Decision Flow

When the proxy receives a CONNECT request (HTTPS), it takes the target domain from the CONNECT authority. A target with userinfo (`@`), an empty host, an explicit port that is not a valid port number, a bracketed host that is not an IPv6 address, or an unspecified host (`0.0.0.0`, `::` in any bracketed spelling such as `[::]` or `[0::0]`, or an IPv4-mapped form such as `[::ffff:0.0.0.0]`) answers `400` before any rule is evaluated and before anything is logged. A target without a port, or with an empty one (`CONNECT host:`), defaults to 443. Then:

1. If the domain matches a credential route and MITM is unavailable, answer `502` and close the connection.
2. If the domain matches a credential route, or `domain_needs_mitm(domain)` is true, and MITM is available, perform MITM TLS interception: terminate the client TLS session with a generated leaf certificate and handle each decrypted request (see [MITM Behavior](#mitm-behavior)). A request to a credential route is tunnelled to the daemon before any rule is evaluated (see [Credential Protection](credential-protection.md)); every other request is evaluated with `evaluate(domain, path)`.
3. If `domain_needs_mitm(domain)` is true but MITM is unavailable, call `evaluate_domain_only(domain)`, which skips path-bearing `block` and `allow` rules and turns a matching `respond` rule, whatever its `paths`, into a block that applies ahead of any later rule for the domain. The verdict is applied at the connection level, so the CONNECT is refused.
4. Otherwise, call `evaluate(domain, "/")` and apply the verdict at the connection level: a block answers `403` and closes the connection, and an allow opens a plain TCP tunnel to the target.

A plain HTTP request (any other method) takes the domain from its absolute-form `http://` target and is evaluated with `evaluate(domain, path)` (see [Plain HTTP](#plain-http)).

## Configuration Reference

Network rules and proxy settings are loaded from three sources, merged at container startup (see [Rule Merging](#rule-merging)):

1. **`cella.toml`** (`.devcontainer/cella.toml`) -- highest precedence on conflicts
2. **`devcontainer.json`** (under `customizations.cella.network`)
3. **`~/.cella/config.toml`** (global) -- lowest precedence

`action = "respond"`, `responses`, and `response_headers` MUST be accepted only in the cella config files, `.devcontainer/cella.{toml,json}` and `~/.cella/config.{toml,json}`. A rule under `customizations.cella.network` that uses any of them MUST fail the config load with an error naming the rule and telling the user to move it to `.devcontainer/cella.toml`. That layer is itself a merge of several files, so a relative `body_file` in it would have no reliable base directory.

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
action = "block"                     # "block" | "allow" | "respond", required

[[network.rules]]
domain = "api.example.com"
paths = ["/v1/admin/*", "/internal/**"]  # string[], optional
action = "block"

[[network.rules]]
domain = "api.anthropic.com"
paths = ["/v1/messages"]
action = "respond"

[[network.rules.responses]]
status = 529
headers = { content-type = "application/json", retry-after = "0" }
body = '{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}'

[[network.rules.responses]]
status = 200
headers = { content-type = "application/json" }
body_file = "fixtures/message.json"

[[network.rules]]
domain = "api.example.com"
action = "allow"
response_headers = { x-test-run = "1" }
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
| `network.rules[].action` | `"block" \| "allow" \| "respond"` | -- | yes | Whether this rule blocks, allows, or answers matching traffic |
| `network.rules[].responses` | `ResponseSpec[]` | `[]` | with `respond` | Responses a `respond` rule serves in order; the last one repeats |
| `network.rules[].responses[].status` | `u16` | -- | yes | Status code, `200..=599` |
| `network.rules[].responses[].headers` | `map<string, string>` | `{}` | no | Response headers; no `content-type` is added automatically |
| `network.rules[].responses[].body` | `string?` | `null` | no | Inline body; with neither `body` nor `body_file` the body is empty |
| `network.rules[].responses[].body_file` | `string?` | `null` | no | File whose content becomes the body; read on the host and inlined |
| `network.rules[].response_headers` | `map<string, string>` | `{}` | no | Headers an `allow` rule sets on upstream responses, replacing upstream values of the same name |

Unknown fields in `[network]`, `[network.proxy]`, `[[network.rules]]`, and `[[network.rules.responses]]` MUST be rejected (`deny_unknown_fields`).

### Respond and Response Header Validation

The host MUST validate `respond` rules and `response_headers` when the config is loaded, before anything reaches the agent. Each error names the rule by its domain and paths, not by index, because indices are post-merge and would not match the user's file.

- `action = "respond"` MUST have a non-empty `responses`, and any other action with `responses` is an error.
- `response_headers` is only valid with `action = "allow"`.
- `status` MUST be in `200..=599`. A `204`, `205`, or `304` response MUST NOT have a non-empty `body` or any `body_file`, even an empty file.
- `body` and `body_file` are mutually exclusive.
- A `body_file` MUST be valid UTF-8 and at most 1 MiB.
- Header names in `headers` and `response_headers` MUST be RFC 9110 tokens of at most 256 bytes (`cella_network::validate::MAX_HEADER_NAME_LEN`). Names are compared case-insensitively, and a name repeated in a different case is an error.
- Header values MUST contain only bytes `0x20..=0x7E` and HTAB, with no leading or trailing whitespace, and MUST be at most 8192 bytes (`cella_network::validate::MAX_HEADER_VALUE_LEN`).
- `connection`, `keep-alive`, `proxy-connection`, `transfer-encoding`, `upgrade`, `te`, `trailer`, and `content-length` MUST be rejected, because the proxy writes framing headers itself.
- A response's `headers` and a rule's `response_headers` MUST each hold at most 64 headers (`cella_network::validate::MAX_HEADERS`); more is an error at config load.

A relative `body_file` resolves against the directory of the cella config file that declared it: `.devcontainer/` for the project layer, and the directory the global config was loaded from (`~/.cella/`) for the global layer. A leading `~` is expanded. There is no containment restriction, so a `body_file` can name any file the host user can read (see [Limitations](#limitations)). The rewrite happens per layer, after variable substitution and before the layers are merged, and an error names the path as it reads after substitution.

An invalid network config MUST abort `cella up` and compose up; it never falls back to an empty rule set. Compose up validates it before features resolution and `compose build`. `cella network status` and `cella network test` print the diagnostic and exit non-zero. Under `--no-network-rules` the rules are dropped before any `body_file` is read, so a missing or invalid fixture does not block startup.

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

When credential protection is active (see [Credential Protection](credential-protection.md)), requests to credential-protected domains bypass the network proxy's rule evaluation entirely. The agent routes these requests through the credential tunnel to the host daemon, which handles upstream HTTPS independently. Network proxy rules do not apply to credential-tunneled requests, so a `respond` rule or `response_headers` on a credential-protected domain such as `api.anthropic.com` has no effect while credential protection is on. When `cella up` writes credential routes into a new container's proxy config, it MUST print a warning for each `respond` rule or rule with `response_headers` whose domain pattern matches a routed domain, naming the rule and the domain and stating that credential protection takes precedence. Compose up writes no credential routes, so it prints no such warning. When rules are configured, `cella network test` prints a note for such a domain.

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

An IPv6 literal is compared in canonical form: only an IPv4-mapped address (`::ffff:a.b.c.d`) becomes dotted IPv4, and any other address, an IPv4-compatible one such as `::127.0.0.1` included, becomes lowercase compressed IPv6 (`::7f00:1`), without brackets. A rule domain written as an IPv6 literal, with or without brackets, compiles to the same canonical form. The agent canonicalises a bracketed target host the same way before rule matching, credential lookup, and connecting, while `Host` keeps the bracketed form as written.

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
| `evaluate(domain, path)` | Domain + request path | Evaluates all rules (domain-only and path-bearing) | MITM'd HTTPS requests, plain HTTP requests, CONNECT-level decisions for domains that need no MITM (with path `/`) |
| `evaluate_domain_only(domain)` | Domain only | Skips `block` and `allow` rules that have path patterns; a matching `respond` rule blocks | CONNECT-level decisions where MITM is needed but unavailable |

`evaluate_domain_only` MUST NOT evaluate path-bearing rules against a synthetic path. Path-bearing `block` and `allow` rules are skipped entirely -- they do not contribute to the verdict. A `respond` rule whose domain matches, whatever its paths, MUST produce a block with reason `respond rule requires TLS interception: <rule>`, so a `respond` rule fails closed instead of tunnelling to the real server. The block applies to the whole domain and takes effect ahead of any later rule for that domain, because the first matching rule decides.

### Rule Verdict

Every evaluation produces a `RuleVerdict`:

| Field | Type | Description |
|---|---|---|
| `action` | `VerdictAction` | What to do with the request; `is_allowed()` is true for `Allow` |
| `reason` | `string` | Human-readable explanation (e.g., "blocked by rule: *.prod.internal (block)") |
| `matched_rule` | `string?` | The pattern display string of the matching rule (e.g., `api.anthropic.com /v1/messages (respond)`), or `null` if the mode default applied |
| `source` | `string?` | Source label of the matching rule (`"config"` for rules built by `RuleMatcher::new`), or `null` if the mode default applied |

`VerdictAction` is one of:

| Variant | Meaning |
|---|---|
| `Allow { response_headers }` | Forward upstream; `response_headers` (or `None`) are set on the response |
| `Block` | Refuse the request |
| `Respond(Arc<RespondRule>)` | Answer with the rule's next configured response; the `RespondRule` is shared, so its sequence position advances across every request that matches it |

The reason of a matched rule is `allowed by rule: <rule>`, `blocked by rule: <rule>`, or `responded by rule: <rule>`.

### MITM Predicate

`domain_needs_mitm(domain)` returns `true` if any rule for the given domain has path patterns, is a `respond` rule, or has `response_headers`. This predicate drives the agent's per-domain MITM decision:

- `true`: The proxy MUST perform TLS interception to read the request or write the response before rendering a verdict.
- `false`: A domain-level decision is sufficient; no MITM is needed. The proxy blocks or allows at the CONNECT level.

On the host, `NetworkConfig::needs_mitm(credentials_protect)` decides whether the CA is generated and shipped to the agent: it is `true` when any rule has path patterns, is a `respond` rule, or has `response_headers`, or when credential protection is on.

## Respond Rules and Response Headers

### Respond Rules

A request that matches a `respond` rule is answered by the agent without contacting the upstream; no upstream connection is opened for it. `respond` rules apply to plain HTTP requests and to HTTPS requests decrypted by the MITM path.

- **Sequence:** responses are served in order and the last one repeats. The position is an atomic counter in the agent process, shared by every connection and request that matches the rule, and it resets when the agent restarts. Every matching request advances it, including `HEAD` and CORS preflight `OPTIONS` requests, because rules match on domain and path and never on method. The step is consumed only when a response is actually produced, so a client that disconnects while its request body is being drained does not consume one. A `paths` pattern that excludes the preflight's path, or an earlier `allow` rule matching it, keeps preflights out of the sequence; an `allow` rule applies to every method on that path.
- **`request-id`:** every served response MUST carry a `request-id` header. When the configured headers do not set one (compared case-insensitively), the agent generates `req_cella_<uuid>` (a UUID v4 in simple form), unique per served response. A configured `request-id` is sent once, as written.
- **Headers:** the configured headers are sent as written; the agent adds no `content-type` and sets no `connection` header except as described under request body. hyper writes the framing headers and writes a status line for codes without a canonical reason phrase, such as `529`.
- **`HEAD`:** a `HEAD` request gets the configured headers with `content-length` equal to the length of the configured body, and no body. A `204` or `304` response gets no added `content-length`.
- **Request body:** a request with `Expect: 100-continue` is answered at once without reading its body, and no `100 Continue` is sent. Any other request body is read and discarded, up to 64 MiB or 10 seconds, before the response goes out, so a client still sending a body reads the response instead of a reset. When the body is not fully read (`Expect`, the cap, or the timeout), an HTTP/1.x response carries `connection: close`, and on HTTP/2 the stream is reset after the response.
- **Invalid response:** a configured response that cannot be built in the agent yields `502` and an `ERROR` log line; host validation makes this unreachable in practice.
- **Upgrades:** rules are evaluated before the upgrade check, so `respond` and `block` also answer upgrade requests.

### Response Headers

A request that matches an `allow` rule with `response_headers` is forwarded, and each configured header is set on the response, replacing any upstream header of the same name rather than appending to it. This applies to plain HTTP and MITM'd HTTPS responses, including the response to a forwarded upgrade request. The headers are set only on responses that came from the upstream, not on error responses the proxy produces itself, such as a `502`. Header injection never generates a `request-id`; a `request-id` configured in `response_headers` is set like any other header.

### Request Log

Every decision is appended to `/tmp/.cella/proxy.log` as a tab-separated line:

| Outcome | Line |
|---|---|
| Allowed | `{ts}\tALLOWED\t{domain}\t{path}` |
| Blocked | `{ts}\tBLOCKED\t{domain}\t{path}\t{reason}` |
| Responded | `{ts}\tRESPONDED\t{domain}\t{path}\t{status}\t{rule}` |
| Error | `{ts}\tERROR\t{domain}\t{error}` |

`{rule}` is the matched rule's pattern display, such as `api.anthropic.com /v1/messages (respond)`.

## Plain HTTP

Each client connection to the proxy is served by hyper as HTTP/1.1. Every request that is not a CONNECT is a plain HTTP proxy request, and every request on a keep-alive connection is evaluated against the rules on its own.

- **Target:** only absolute-form `http://` targets are accepted. Any other target (origin-form, or `https://` in absolute form) receives `400`. The `Host` header MUST NOT be used as a fallback, since `Host: 127.0.0.1:<proxy_port>` would make the proxy connect to itself.
- **Authority:** an explicit port that is not a valid port number (such as `:65536`, `:abc`, or `:+80`) receives `400`; the default port 80 applies when the target has no port or an empty one (`http://host:/`). An empty host (`http://:80/x`) receives `400`, as does an unspecified host (`0.0.0.0`, `::` in any bracketed spelling such as `[::]` or `[0::0]`, or an IPv4-mapped form such as `[::ffff:0.0.0.0]`). Alternate IPv4 spellings of an address (see [Limitations](#limitations)) are not recognised as IP literals by the authority parser. A bracketed host that is not an IPv6 address, or that carries a zone ID, receives `400`. A bracketed IPv6 host is unbracketed and canonicalised (see [Domain Patterns](#domain-patterns)) for rule matching and connecting, and `Host` keeps the bracketed form.
- **Header text:** a request whose `Connection` or `Upgrade` field lines are not valid visible-ASCII header text receives `400` before any rule decision, since such a line could hide an `Upgrade` token or a header listed for stripping.
- **Evaluation:** `evaluate(domain, path)` with the query string and fragment stripped from the path. A block answers `403`; a `respond` rule answers with its next response.
- **Forwarding without an upstream proxy:** the request goes out in origin-form with `Host` set to the target authority (RFC 9112 §3.2.2), which also covers HTTP/1.0 clients that omit `Host`. Upstream connections are reused within one client connection, keyed by lowercased host and numeric port, with at most 8 kept and the least recently used evicted, and a closed upstream connection is reopened for the next request. `Host` is built from host and port only, so any userinfo in the request URI is dropped.
- **Forwarding through an upstream proxy:** the agent opens a `CONNECT host:port` tunnel through the configured upstream proxy and sends the request in absolute-form inside it, with `Host` likewise set to the target authority and no userinfo in the request target.
- **Hop-by-hop headers:** stripped from requests and responses in both directions, including `connection`, every header the `connection` header lists across all of its field lines (RFC 9110 §7.6.1), and `proxy-authorization`.
- **`Expect: 100-continue`:** hyper answers it itself, so an upstream's own `100` or `417` is not relayed.
- **Limits:** a request head is capped at 100 headers and about 408 KiB.
- **Upgrades:** only WebSocket upgrades (such as `ws://`) are relayed, on a dedicated upstream connection. The proxy offers the upstream `Upgrade: websocket` only, dropping other tokens and stripping `HTTP2-Settings`, and splices the connection only when the upstream answers `101` with `Upgrade: websocket` as its sole token. Any other `101` receives a `502` and the connection closes. A non-WebSocket upgrade request, such as `h2c`, is forwarded as an ordinary request with `Upgrade`, `HTTP2-Settings`, and the `upgrade` Connection token stripped, so later requests on the connection are still evaluated. A non-`101` reply to an upgrade request has its hop-by-hop headers stripped. A `101` whose `Upgrade` line is not valid visible-ASCII text is not treated as a WebSocket switch. The MITM path applies the same rules (see [MITM Behavior](#mitm-behavior)).

## TLS Interception

### CA Lifecycle

When `NetworkConfig::needs_mitm` is true (a rule has path patterns, is a `respond` rule, or has `response_headers`, or credential protection is on), cella generates a self-signed CA certificate for MITM TLS interception. The CA is stored on the host:

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

The CA certificate and key are embedded in the agent's proxy config JSON (`CELLA_PROXY_CONFIG` env var) only when `NetworkConfig::needs_mitm` is true:

```json
{
  "listen_port": 18080,
  "mode": "denylist",
  "rules": [
    {"domain": "*.production.example.com", "action": "block"},
    {
      "domain": "api.anthropic.com",
      "action": "respond",
      "paths": ["/v1/messages"],
      "responses": [
        {"status": 529, "headers": {"content-type": "application/json"}, "body": "{...}"},
        {"status": 200, "headers": {"content-type": "application/json"}, "body": "{...}"}
      ]
    },
    {"domain": "api.example.com", "action": "allow", "response_headers": {"x-test-run": "1"}}
  ],
  "upstream_proxy": "http://proxy.corp:3128",
  "ca_cert_pem": "-----BEGIN CERTIFICATE-----\n...",
  "ca_key_pem": "-----BEGIN PRIVATE KEY-----\n..."
}
```

Each rule object carries `domain` and `action`, plus `paths`, `responses`, and `response_headers` when they are non-empty. A response's `body` is the inline body or the content of its `body_file`, inlined on the host; the agent never reads `body_file` itself. The agent maps an unrecognised `action` to `block` with a warning, so an older agent given a `respond` rule fails closed.

Otherwise, `ca_cert_pem` and `ca_key_pem` are `null`. The credential protection system extends this same JSON object with `credential_routes`, `daemon_addr`, `container_nonce`, and `container_name` fields (see [Credential Protection](credential-protection.md)).

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
3. Read each plaintext HTTP request (method, path, headers)
4. If the domain matches a credential route, tunnel the request to the daemon without evaluating rules
5. Otherwise, evaluate the request against the rule set via `evaluate(domain, path)`
6. If blocked, return `403` over the MITM'd connection
7. If a `respond` rule matched, return its next response (see [Respond Rules](#respond-rules))
8. If allowed, forward the request to the upstream server (or through the upstream proxy if configured) and set the rule's `response_headers` on the response

The upstream connection is opened lazily, on the first request that is forwarded. A connection whose requests are all answered by `respond` rules never opens an upstream socket, and neither does a connection whose requests all go to the credential tunnel. A failed lazy connect returns `502` for that request and writes an `ERROR` log line.

Upstream connections belong to one client connection. An HTTP/2 upstream is shared by all streams. With an HTTP/1.1 upstream, a connection busy with an earlier response does not block other streams: another upstream connection is opened, up to 8 extra per client connection, so at most 9 in all. When all 9 are busy, further requests wait for one to come free rather than receiving `502`.

A request whose `Connection` or `Upgrade` field lines are not valid visible-ASCII header text receives `400` before the credential-route check and before any rule decision.

Upgrades follow the same WebSocket-only rule as [Plain HTTP](#plain-http). Only a request whose `Upgrade` tokens include `websocket` is relayed, on a dedicated upstream connection, with the outgoing `Upgrade` rewritten to exactly `websocket` and `HTTP2-Settings` stripped. The connection is spliced only when the upstream answers `101` with `Upgrade: websocket` as its sole token; any other `101` receives a `502` and the connection closes. Any other upgrade request, such as `h2c`, is forwarded as an ordinary request with `Upgrade`, `HTTP2-Settings`, and the `upgrade` Connection token stripped.

Hop-by-hop headers, including those named in `Connection`, are stripped from upstream responses as they are on plain HTTP, except on a relayed `101`.

Domain-only `block` and `allow` rules without `response_headers` do not require MITM. The proxy blocks at the CONNECT level by answering `403` and closing the connection before any TLS handshake occurs.

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

When the agent proxy is active (any network rule is configured, or credential protection is on), proxy env vars point to the cella-agent's local proxy (`http://127.0.0.1:<proxy_port>`) instead of the upstream proxy. This ensures all traffic routes through the rule engine.

When the agent proxy is not active, proxy env vars point directly to the upstream proxy (or the auto-detected host proxy).

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

Network configuration is part of the cella config, which is assembled from up to three layers before it is deserialized:

1. `~/.cella/config.{toml,json}` (global, lowest priority)
2. `customizations.cella` in the resolved devcontainer configuration
3. `.devcontainer/cella.{toml,json}` in the workspace (project, highest priority)

`~/.cella/config.*` is the global cella config and is unrelated to `~/.config/cella/global.jsonc`, which is a global devcontainer.json layer merged into the devcontainer configuration before `customizations.cella` is read from it.

The layers are deep-merged field by field as JSON values:

- Objects merge key by key, so `[network.proxy]` fields set in different layers combine and a higher layer replaces only the keys it sets.
- Scalars such as `mode` are replaced by the highest layer that sets them.
- Arrays are concatenated with the higher layer's items first, so `network.rules` lists the project `cella.toml` rules first, then the `customizations.cella` rules, then the `~/.cella/config.*` rules.

Rules are not deduplicated. Combined with first-match-wins evaluation, a project rule takes precedence over any lower-layer rule that matches the same request, whether or not the domain strings are identical.

## CLI Interface

### `cella network status`

Display the current proxy and blocking configuration for a running container.

```
$ cella network status
Proxy: active (localhost:18080)
Upstream HTTP: http://proxy.corp:3128
Upstream HTTPS: http://proxy.corp:3128
CA: auto-generated (~/.cella/proxy/ca.pem)
Mode: denylist (5 rules)
  block: *.production.example.com
  block: api.example.com [/v1/admin/*, /internal/**]
  allow: registry.npmjs.org
  respond: api.anthropic.com [/v1/messages] (529 → 200)
  allow: api.staging.example.com (+1 response headers)
```

A `respond` rule line ends with its response statuses in serving order, joined by `→`. An `allow` rule with `response_headers` ends with `(+N response headers)`.

### `cella network test <url>`

Test whether a URL would be blocked, allowed, or answered by a `respond` rule under the current rules. Evaluates the URL against the merged rule set, with the query string and fragment stripped from the path as the agent does, and displays the verdict, including which rule matched. The URL is parsed with the agent's authority parser, so bracketed IPv6 hosts, canonicalisation, the default port, and the `400` cases above behave as they do in the proxy: the command evaluates the host the agent would, and reports a URL the agent would reject as rejected.

```
$ cella network test https://api.production.example.com/v1/data
✗ BLOCKED: https://api.production.example.com/v1/data
  blocked by rule: *.production.example.com (block)

$ cella network test https://github.com/user/repo
✓ ALLOWED: https://github.com/user/repo
  allowed (no matching deny rule)

$ cella network test https://api.anthropic.com/v1/messages
↩ RESPONDS: https://api.anthropic.com/v1/messages
  responded by rule: api.anthropic.com /v1/messages (respond)
  sequence: 529, 200

$ cella network test http://example.com:65536/
✗ REJECTED: http://example.com:65536/
  the proxy answers 400 Bad Request: port is not a number from 0 to 65535
```

When credential protection is enabled and the URL's domain is a credential provider domain, the output adds `  note: credential-protected domain; requests go to the credential proxy and rules are not applied`, since such requests never reach the rule engine.

### `cella network log`

Tail the proxy's request log from a running container (see [Request Log](#request-log) for the line format).

```
$ cella network log

# Raw log inside the container
$ cat /tmp/.cella/proxy.log
$ cella exec -- cat /tmp/.cella/proxy.log
```

### `cella up --no-network-rules`

Start a container without enforcing network blocking rules. This sets `NetworkRulePolicy::Skip`, bypassing all configured rules. The rules are dropped before any `body_file` is read and before the respond and header checks run; the `customizations.cella` restriction and unknown-field checks still apply when the config is loaded. Host proxy forwarding still applies if configured.

## Error Handling

### Error Taxonomy

| Condition | Behavior | Diagnostic Code |
|---|---|---|
| Invalid `respond` rule or `response_headers` (missing or misplaced `responses`, status out of range, body on `204`/`205`/`304`, more than 64 headers, header name over 256 bytes or value over 8192 bytes, invalid or forbidden header) | Config load fails; `cella up` and compose up abort, `cella network status`/`test` exit non-zero | `cella::network::missing_responses`, `responses_without_respond`, `response_headers_without_allow`, `invalid_status`, `body_not_allowed`, `invalid_header_name`, `invalid_header_value`, `forbidden_header`, `duplicate_header`, `too_many_headers`, `header_name_too_long`, `header_value_too_long` |
| `body_file` problem (both `body` and `body_file`, `body_file` on `204`/`205`/`304`, unreadable, over 1 MiB, not UTF-8) | Same as above | No diagnostic code; the message names the rule, the response, and the path |
| `respond` fields in `customizations.cella.network` | Same as above; the message asks the user to move the rule to `.devcontainer/cella.toml` | No diagnostic code |
| CA key pair generation fails | MITM unavailable; path rules and `response_headers` are not applied, and `respond` rules block their domain at the CONNECT level | `cella::network::key_generation` |
| CA certificate generation fails | Same degradation as above | `cella::network::cert_generation` |
| CA file I/O failure (read or write) | MITM unavailable; warning logged | `cella::network::io` |
| Additional CA cert unreadable | Warning logged; injection proceeds without the additional cert | `cella::network::read_cert` |
| Container trust store update fails | Warning logged; tools may reject MITM certificates | Non-fatal (command runs with `\|\| true`) |
| MITM certificate rejected by client | Connection fails; tool-specific TLS error | Expected for cert-pinning clients |
| Domain blocked at CONNECT level | HTTP 403 with `connection: close`; no tunnel is opened | Rule verdict logged |
| `respond` rule for a domain without MITM | HTTP 403 to the CONNECT, reason `respond rule requires TLS interception: <rule>` | Rule verdict logged; fail-closed |
| Path blocked after MITM | HTTP 403 over MITM'd connection | Rule verdict logged |
| Plain HTTP request without an absolute-form `http://` target | HTTP 400 | -- |
| Target port that is not a valid port number, an empty host, an unspecified host (`0.0.0.0`, `::` in any bracketed spelling, or an IPv4-mapped form such as `[::ffff:0.0.0.0]`), or a bracketed host that is not an IPv6 address or has a zone ID (plain HTTP and CONNECT) | HTTP 400 before rule evaluation; nothing is logged | -- |
| CONNECT target containing userinfo | HTTP 400 before rule evaluation; nothing is logged | -- |
| Request whose `Connection` or `Upgrade` field lines are not valid visible-ASCII text (plain HTTP and MITM) | HTTP 400 before any rule decision | -- |
| Upstream answers an upgrade request with `101` that is not a sole `Upgrade: websocket` | HTTP 502 and the connection closes | -- |
| Lazy upstream connect fails on the MITM path | HTTP 502 for that request | `ERROR` log line |
| Configured response cannot be built in the agent | HTTP 502 | `ERROR` log line; unreachable after host validation |
| Credential tunnel unavailable for credential domain | HTTP 502; phantom tokens never forwarded upstream | Fail-closed (see [Credential Protection](credential-protection.md)) |
| Upstream proxy unreachable | Connection failure propagated to container process | Host proxy misconfiguration |

### Degradation Behavior

When CA generation fails (key pair or certificate), the host logs a warning:

```
Failed to generate CA for MITM: {error}. TLS interception will be unavailable.
```

The proxy config JSON sets `ca_cert_pem` and `ca_key_pem` to `null`, and the agent operates without MITM capability. For HTTPS traffic to a domain that needs MITM, the agent then uses `evaluate_domain_only`: domain-level `block` and `allow` rules continue to function, rules with path patterns are ignored and `response_headers` are not applied (fail-open), and a `respond` rule blocks the CONNECT (fail-closed). The agent logs one warning per such domain. Plain HTTP traffic needs no MITM and is unaffected.

## Encrypted Client Hello (ECH)

ECH encrypts the TLS SNI field, preventing the proxy from determining the target domain during CONNECT. The proxy uses DNS-based routing as the alternative:

- **DNS-based routing:** DNS queries are intercepted to extract the target domain before the TLS connection begins. Resolved IPs are mapped back to domains for rule evaluation.
- **Config:** `network.proxy.ech_strategy = "dns"` selects the resolution strategy.
- **Fallback:** When ECH is detected and no alternative resolution is available, the mode's default disposition applies (allow for denylist, block for allowlist) and a warning is logged.

## HTTP/2 Inspection

The MITM proxy inspects HTTP/2 traffic at the stream level:

- **Stream-level evaluation:** HTTP/2 frames are demultiplexed and each stream's `:path` pseudo-header is evaluated against the rule set independently.
- **ALPN negotiation:** During the MITM TLS handshake, HTTP/2 is negotiated with the container process and upstream server when both support it.
- **Mixed verdicts:** Different streams on the same connection may receive different verdicts. Blocked streams receive RST_STREAM; allowed streams are forwarded.

Depends on: [IPC Protocol § Transport Upgrade: HTTP/2](ipc-protocol.md#transport-upgrade-http2)

## Per-container Rules

Different network policies apply to different containers within a workspace:

- **Config:** `network.containers.<name>.mode` and `network.containers.<name>.rules` override the global network config for specific containers.
- **Rule merging:** Per-container rules are placed ahead of the workspace rules, so first-match-wins evaluation gives them precedence, the same way project `cella.toml` rules precede lower layers (see [Rule Merging](#rule-merging)).
- **Implementation:** The agent receives its container-specific merged rule set via `CELLA_PROXY_CONFIG`. No runtime rule resolution is needed.

## QUIC/HTTP/3 Interception

QUIC-based connections are intercepted at the UDP level:

- **UDP interception:** Outbound UDP traffic on port 443 (QUIC) is intercepted. Domain-level rules are applied based on the SNI in the QUIC Initial packet.
- **QUIC MITM:** The QUIC connection is terminated, HTTP/3 frames are inspected, and traffic is forwarded via a separate QUIC connection to the upstream server.
- **Alt-Svc suppression:** `Alt-Svc` headers are stripped from HTTP/1.1 and HTTP/2 responses to prevent clients from upgrading to QUIC, keeping traffic on the interceptable TCP path.

Depends on: [IPC Protocol § Transport Upgrade: QUIC/HTTP/3](ipc-protocol.md#transport-upgrade-quichttp3)

## Multi-label Wildcards

The `**` wildcard matches zero or more domain labels. `**.example.com` matches `a.example.com` and `a.b.example.com`. Single `*` behavior is unchanged (matches exactly one label).

| Pattern | Matches | Does Not Match |
|---|---|---|
| `**.example.com` | `foo.example.com`, `a.b.example.com` | `example.com` |
| `*` (single label) | `foo.example.com` (one label in position) | `foo.bar.example.com` (two labels in position) |

## Rule Hot-reload

The daemon watches `cella.toml` and `devcontainer.json` for changes. On rule change, the daemon pushes the new merged ruleset to all connected agents via the control connection. No container restart is required. The agent applies the new rules atomically — in-flight requests complete under the old ruleset.

## Trust Store Coverage

Trust store injection covers the following distributions:

| Distribution Family | Package Manager | Update Command |
|---|---|---|
| Debian (Ubuntu, Mint) | apt | `update-ca-certificates` |
| RHEL (Fedora, CentOS, Rocky, Alma) | yum/dnf | `update-ca-trust` |
| Alpine | apk | `update-ca-certificates` |
| Arch | pacman | `update-ca-trust` |
| SUSE (openSUSE) | zypper | `update-ca-trust` |

Custom base images with non-standard trust store locations require manual CA installation.

## Limitations

1. **Certificate pinning** -- Tools that pin TLS certificates (some `curl` builds, mobile SDKs, security-hardened clients) reject MITM certificates. These tools cannot use path-level network filtering.
2. **Credential-routed domains bypass rules** -- With credential protection on, a request to a credential-protected domain goes to the credential tunnel before any rule is evaluated, so `respond` rules and `response_headers` on such a domain (for example `api.anthropic.com`) have no effect. `cella up` prints a warning for each such rule when it creates the container, naming the rule and the domain and stating that credential protection takes precedence.
3. **Sequence position is per agent process** -- A `respond` rule's sequence position is shared by every connection and request that matches the rule, so concurrent clients advance the same sequence, and it resets when the agent restarts.
4. **Respond fields only in cella config files** -- `action = "respond"`, `responses`, and `response_headers` are rejected under `customizations.cella.network`; they belong in `.devcontainer/cella.toml` or `~/.cella/config.toml`.
5. **`body_file` reads any host file** -- `body_file` has no containment restriction, so a repository's `.devcontainer/cella.toml` can copy any file the host user can read into the container's proxy config, from where code in the container can request it through the proxy.
6. **Alternate IPv4 spellings are not matched against IP rules** -- The authority parser rejects `0.0.0.0`, `::`, and their IPv4-mapped forms (such as `::ffff:0.0.0.0`) with `400`, but other spellings of an IPv4 address (`0`, `0x0`, `127.1`, `2130706433`) are treated as hostnames and resolved by the system resolver. They are not yet normalised to dotted IPv4, so a rule written for the address (for example `127.0.0.1`) does not match a request that uses one of them.
