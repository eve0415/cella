# Network Proxy

Cella runs a transparent MITM proxy inside dev containers that can block or allow network traffic at the domain and path level, answer matching requests with configured mock responses, and add headers to real responses. Rules are glob-based, support HTTPS interception for decisions that need the request or response, and merge from three configuration sources. When no rules are configured, host proxy settings are forwarded through to the container unchanged.

## Configuration

Network rules and proxy settings come from three sources:

1. **`cella.toml`** (project-level, `.devcontainer/cella.toml`) -- takes precedence on conflict
2. **`devcontainer.json`** (under `customizations.cella.network`)
3. **`~/.cella/config.toml`** (global) -- lowest precedence

`respond` rules and the `responses` and `response_headers` fields are accepted only in the cella config files, `.devcontainer/cella.toml` and `~/.cella/config.toml` (or their `.json` forms). Using them under `customizations.cella.network` is a load error that asks you to move the rule to `.devcontainer/cella.toml`.

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

cella merges its config from `~/.cella/config.toml` (global), `customizations.cella` in `devcontainer.json`, and `.devcontainer/cella.toml` (project), in increasing order of priority:

- **Mode** and other single values: the highest-priority source that sets them wins.
- **Proxy settings**: merged field by field, so a `cella.toml` that sets only `http` keeps the `no_proxy` from `devcontainer.json`.
- **Rules**: all rules are kept, with `cella.toml` rules first, then `devcontainer.json` rules, then global rules. The first matching rule wins, so a `cella.toml` rule overrides any lower-priority rule that matches the same request.

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

An IPv6 literal is matched in its canonical form, so `[2001:DB8::1]`, `2001:db8::1`, and `[2001:db8:0:0:0:0:0:1]` are the same rule domain, and an IPv4-mapped address such as `[::ffff:127.0.0.1]` matches as `127.0.0.1`. Only IPv4-mapped addresses are converted: an IPv4-compatible address such as `[::127.0.0.1]` stays IPv6 and matches as `::7f00:1`. The proxy answers `400` to a request whose target has a port that is not a valid port number (such as `:65536`, `:abc`, or `:+80`), an empty host, a bracketed host that is not an IPv6 address or carries a zone ID, the unspecified address `0.0.0.0` or `::` (`[::]`, `[0::0]`) or its IPv4-mapped form (`[::ffff:0.0.0.0]`), or, for `CONNECT`, userinfo (`user@host`). The default port (80 for `http://`, 443 for `CONNECT`) applies when the target names no port or an empty one (`http://host:/`, `CONNECT host:`).

Alternate IPv4 spellings such as `0`, `0x0`, `127.1`, and `2130706433` are not parsed as addresses. The system resolver resolves them, and they are not yet matched against IP rules, so a rule written for `127.0.0.1` does not apply to a request that uses one of them.

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

cella must perform TLS interception (MITM) for HTTPS requests to a domain when any rule for that domain has path patterns, is a `respond` rule, or sets `response_headers`, because it has to read the request path or write the response itself. Domain-only `block` and `allow` rules without `response_headers` do not require MITM -- the proxy decides at the connection level.

If TLS interception is unavailable (for example, the CA could not be created), path rules and `response_headers` are not applied to HTTPS traffic, and a `respond` rule, even one scoped by `paths`, makes the whole domain fail closed: the `CONNECT` is refused, ahead of any later rule for that domain, instead of letting requests reach the real server.

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

## Mock responses

A `respond` rule answers matching requests itself, without contacting the upstream server. Use it to test how code inside the container handles errors, retries, and success against an API without the request reaching the real server. `respond` rules apply to plain HTTP requests and to HTTPS requests the proxy decrypts.

The following `.devcontainer/cella.toml` makes the first request to the Anthropic Messages API fail with `529 Overloaded` and every later request succeed, which exercises a client's retry handling:

```toml
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
```

The `body_file` path resolves to `.devcontainer/fixtures/message.json`, next to the `cella.toml` that names it.

`api.anthropic.com` is a built-in credential-protected domain, so this rule only takes effect while credential protection is off (see [Credential-protected domains](#credential-protected-domains)). When credential protection is on and an Anthropic API key is forwarded, the rule does not apply, and `cella up` prints a warning that names the rule and the domain.

### Response fields

| Field | Type | Description |
|-------|------|-------------|
| `status` | integer | HTTP status code, `200` to `599` (required) |
| `headers` | table of strings | Response headers |
| `body` | string | Response body |
| `body_file` | string | Path to a file whose content becomes the response body |

`body` and `body_file` are mutually exclusive; with neither, the body is empty. A `204`, `205`, or `304` response cannot have a non-empty `body` or any `body_file`.

cella sends the headers you configure plus the framing headers the proxy writes itself, such as `content-length`. It does not add a `content-type`, so set one yourself as in the example.

A `HEAD` request gets the same headers, with `content-length` set to the length of the configured body and no body. A `204` or `304` response carries no `content-length`.

### Sequences

Responses are served in order, and the last one repeats for every later request. The position in the sequence is kept by the proxy inside the container: it is shared by every connection and request that matches the rule, and it starts over when the proxy restarts, for example when the container restarts.

Every request that matches the rule advances the sequence, including `HEAD` requests and CORS preflight `OPTIONS` requests, because rules match on domain and path and never on method. A step is consumed only when a response is actually produced, so a client that disconnects while the proxy is still draining its request body does not use one up. To keep preflights out of the sequence, narrow `paths` so they do not match the preflight's path, or put an `allow` rule for that path ahead of the `respond` rule. The `allow` rule applies to every method on that path, so the `respond` rule stops answering the real requests there as well.

### `request-id`

Every response a `respond` rule serves carries a `request-id` header, because API clients expect one on a response that comes from the API. When the response's `headers` do not set `request-id` (compared case-insensitively), cella generates a unique `req_cella_<uuid>` value for each response served. A configured `request-id` is sent once, exactly as written.

### Request bodies

The proxy reads and discards the request body, up to 64 MiB or 10 seconds, before it sends the configured response, so a client still uploading a large request body receives the response rather than a connection reset. A request with `Expect: 100-continue` is answered at once without reading its body, and no `100 Continue` is sent. When the body is not fully read, whether because of `Expect`, the 64 MiB cap, or the timeout, an HTTP/1.x response carries `connection: close`, and on HTTP/2 the stream is reset after the response.

### `body_file`

A relative `body_file` resolves against the directory of the config file that declared it: `.devcontainer/` for `.devcontainer/cella.toml`, and `~/.cella/` for `~/.cella/config.toml`. A leading `~` expands to your home directory. The file must be valid UTF-8 and at most 1 MiB. cella reads it on the host and copies its content into the proxy config, so the file itself is never mounted into the container.

> **Security:** `body_file` can name any file the host user can read; it is not restricted to the workspace. A repository's `.devcontainer/cella.toml` can therefore copy a host file such as an SSH key or a cloud credential into the container, where any code running there can request it through the proxy. Review the `body_file` entries of a repository you do not trust before running `cella up` in it, or start it with `cella up --no-network-rules`, which skips the rules before any `body_file` is read.

### Validation

cella checks `respond` rules and `response_headers` on the host when it loads the config:

- A `respond` rule needs at least one entry in `responses`, and `responses` is only valid on a `respond` rule.
- `status` must be between `200` and `599`.
- Header names must be valid HTTP tokens and are compared case-insensitively, so the same name in two cases is rejected.
- Header values may contain only visible ASCII characters, spaces, and tabs, with no leading or trailing whitespace.
- `connection`, `keep-alive`, `proxy-connection`, `transfer-encoding`, `upgrade`, `te`, `trailer`, and `content-length` cannot be configured, because the proxy manages them.
- A response's `headers` and a rule's `response_headers` each hold at most 64 headers, a header name is at most 256 bytes, and a header value is at most 8192 bytes.

An invalid rule stops `cella up`, including compose-based containers before any image is built, with an error that names the rule by its domain and paths. `cella network status` and `cella network test` report the same error and exit non-zero.

### Credential-protected domains

When credential protection is on, requests to a credential-protected domain (such as `api.anthropic.com`) go to the credential proxy before any network rule is evaluated, so `respond` rules and `response_headers` have no effect on them. When `cella up` creates a container with credential routes, it prints a warning for each such rule that names the rule and the domain and says credential protection takes precedence. When rules are configured, `cella network test` prints a note for such a domain.

## Response headers

An `allow` rule can set headers on the responses the upstream server returns for matching requests:

```toml
[[network.rules]]
domain = "api.example.com"
action = "allow"
response_headers = { x-test-run = "1" }
```

Each configured header replaces any header of the same name that the upstream sent. The headers are added only to responses that came from the upstream, not to error responses the proxy produces itself, such as a `502`. `response_headers` never generates a `request-id`; a `request-id` you configure in `response_headers` is set like any other header. `response_headers` is only valid on `allow` rules, follows the same header rules as `respond` responses, and needs TLS interception for HTTPS traffic.

## CA Certificates

When a rule requires HTTPS interception (path patterns, `respond`, or `response_headers`) or credential protection is on, cella auto-generates a CA certificate and private key:

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
Mode: denylist (5 rules)
  block: *.production.example.com
  block: api.example.com [/v1/admin/*, /internal/**]
  allow: registry.npmjs.org
  respond: api.anthropic.com [/v1/messages] (529 → 200)
  allow: api.staging.example.com (+1 response headers)
```

A `respond` rule lists the statuses of its responses in serving order, and an `allow` rule with `response_headers` shows how many headers it sets.

### `cella network test <url>`

Test whether a URL would be blocked, allowed, or answered by a `respond` rule under the current rules. The query string is ignored, as it is by the proxy. The URL is parsed with the same authority parser the proxy uses, so a bracketed IPv6 host, its canonical form, and the default port are evaluated exactly as the proxy would, and a URL the proxy would answer with `400` is reported as rejected.

```sh
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

When credential protection is on and the URL's domain is credential-protected, the output adds `note: credential-protected domain; requests go to the credential proxy and rules are not applied`.

### `cella network log`

Tail the proxy's request log from a running container. Each line is tab-separated and starts with a timestamp and an outcome: `ALLOWED`, `BLOCKED`, `RESPONDED`, or `ERROR`. A `RESPONDED` line records the domain, the path, the status served, and the rule that served it.

```sh
# From the host
$ cella network log

# Or read the raw file directly (inside the container, or via exec)
$ cat /tmp/.cella/proxy.log
$ cella exec -- cat /tmp/.cella/proxy.log
```

## Disabling

To start a container without any network blocking rules:

```sh
$ cella up --no-network-rules
```

This bypasses all configured rules, and no `body_file` is read. Host proxy forwarding still applies if configured.

## Proxy Port

The in-container proxy listens on port 18080 by default. To change it:

```toml
[network.proxy]
proxy_port = 19090
```

The proxy is started when any network rule is configured or credential protection is on. Without either, proxy environment variables point directly to the upstream proxy (or are unset).
