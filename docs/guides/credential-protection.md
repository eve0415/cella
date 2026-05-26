# Credential Protection

Dev containers run untrusted code -- compromised npm packages, malicious VS Code extensions, backdoored devcontainer features. Any of these can read environment variables and exfiltrate your API keys in seconds. Credential protection stops this by keeping real credentials on your host machine and injecting opaque placeholders (phantom tokens) into the container instead.

## Enabling credential protection

Add this to your project's `.devcontainer/cella.toml`:

```toml
[credentials]
protect = true
```

That's it. On the next `cella up`, your container will receive phantom tokens instead of real API keys.

## How it works

1. When you run `cella up`, cella generates a random UUID placeholder for each credential (e.g., `pt-550e8400-e29b-41d4-a716-446655440000`)
2. These phantom tokens are injected into the container as environment variables -- your code sees `ANTHROPIC_API_KEY` but the value is meaningless outside the host daemon
3. When a process in the container makes an API request (e.g., to `api.anthropic.com`), the in-container proxy intercepts it and tunnels it to the host daemon
4. The daemon swaps the phantom token for the real credential and makes the upstream request
5. The response flows back through the tunnel to the container process

Your code doesn't need any changes. SDKs read the env var, include it in the request header, and the proxy handles the rest transparently.

## Built-in providers

These 12 providers work out of the box. Each one activates automatically when the corresponding environment variable is set on your host:

| Provider | Env var | API domain |
|----------|---------|------------|
| GitHub | `GH_TOKEN`* | `github.com`, `api.github.com` |
| Anthropic | `ANTHROPIC_API_KEY` | `api.anthropic.com` |
| OpenAI | `OPENAI_API_KEY` | `api.openai.com` |
| Gemini | `GEMINI_API_KEY` | `generativelanguage.googleapis.com` |
| Groq | `GROQ_API_KEY` | `api.groq.com` |
| Mistral | `MISTRAL_API_KEY` | `api.mistral.ai` |
| DeepSeek | `DEEPSEEK_API_KEY` | `api.deepseek.com` |
| xAI | `XAI_API_KEY` | `api.x.ai` |
| Fireworks | `FIREWORKS_API_KEY` | `api.fireworks.ai` |
| Together | `TOGETHER_API_KEY` | `api.together.xyz` |
| Perplexity | `PERPLEXITY_API_KEY` | `api.perplexity.ai` |
| Cohere | `COHERE_API_KEY` | `api.cohere.com` |

*GitHub credentials are resolved via `gh auth token`, not by reading `GH_TOKEN` directly. You need `gh auth login` on your host.

## Custom providers

For internal APIs or providers not in the built-in list, add a `[[credentials.providers]]` entry:

```toml
[credentials]
protect = true

[[credentials.providers]]
name = "internal-api"
env = "INTERNAL_API_KEY"
domains = ["api.internal.corp"]
header = "Authorization"
prefix = "Bearer "
```

| Field | Required | Description |
|-------|----------|-------------|
| `name` | yes | Short identifier for the provider |
| `env` | yes | Host environment variable holding the real credential |
| `domains` | yes | Target API domains (array) |
| `header` | yes | HTTP header name for credential injection |
| `prefix` | no | Header value prefix (e.g., `"Bearer "`) |

The first time cella encounters a custom provider, it prompts you to approve it:

```
Custom credential provider requires approval:
  Name:   internal-api
  Env:    INTERNAL_API_KEY
  Domain: api.internal.corp
This will send the value of INTERNAL_API_KEY to api.internal.corp.
Approve? [y/N]
```

This consent flow prevents malicious `cella.toml` files from silently routing your credentials to attacker-controlled domains. Approvals are stored in `~/.cella/approved-providers.json` and persist across projects. If any field of the provider changes, you'll be prompted again.

## Disabling specific providers

To disable a built-in AI provider, set it to `false` under `[credentials.ai]`:

```toml
[credentials]
protect = true

[credentials.ai]
openai = false    # Don't protect/forward OpenAI credentials
groq = false      # Don't protect/forward Groq credentials
```

To disable GitHub credential forwarding entirely:

```toml
[credentials]
gh = false
```

When a provider is disabled, no phantom token is generated for it and the credential is not forwarded to the container at all.

## Troubleshooting

### All API calls return 502

The host daemon is unreachable or has restarted since the container was created.

- Check the daemon is running: `ls ~/.cella/daemon.pid`
- Rebuild the container: `cella up --rebuild`

### Specific provider returns 403

The phantom token isn't registered or the real credential is unavailable.

- Verify the env var is set on your host: `echo $ANTHROPIC_API_KEY`
- Check the audit log: `tail ~/.cella/credential-audit.log`
- Look for a `denial_reason` in the log entry

### GitHub operations fail with 403

The `gh` CLI can't authenticate on the host.

- Run `gh auth status` on the host to verify your login
- Re-authenticate if needed: `gh auth login`
- If using GitHub Enterprise, ensure `gh auth login -h your-ghe-host.com` is configured

### Custom provider not working

User consent may not have been granted.

- Check `~/.cella/approved-providers.json` for your provider
- If the provider definition changed since approval, you'll need to re-approve on the next `cella up`

### Slow API calls

Credential resolution adds a small overhead, cached for 60 seconds by default. If every request is slow:

- Ensure `cache_ttl_seconds` isn't set to `0` (which disables caching)
- Check `duration_ms` in the audit log to isolate whether the slowdown is credential resolution or upstream latency

## Further reading

For the full technical specification including the wire protocol, threat model, and security invariants, see the [credential protection spec](../specs/credential-protection.md).
