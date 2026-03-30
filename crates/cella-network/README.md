# cella-network

> Network proxy configuration, domain/path blocking rules, and CA certificate management.

Part of the [cella](../../README.md) workspace.

## Overview

cella-network provides the configuration types, rule engine, and CA infrastructure for controlling network access inside dev containers. It supports two blocking modes: denylist (block matching traffic, allow everything else) and allowlist (allow matching traffic, block everything else). Rules use glob patterns for both domain and path matching, enabling fine-grained control like blocking `*.prod.internal` or only `/admin/**` paths on a specific API domain.

Proxy environment variables (`HTTP_PROXY`, `HTTPS_PROXY`, `NO_PROXY`) are auto-detected from the host and forwarded into containers with safety entries to prevent proxy loops. When path-level blocking rules are active, the crate generates a self-signed CA certificate for MITM TLS interception, stored at `~/.cella/proxy/`. Configuration merges from `devcontainer.json` customizations and `cella.toml`, with `cella.toml` taking precedence on conflicts.

## Architecture

### Key Types

- `NetworkConfig` -- top-level configuration loaded from `cella.toml` or `customizations.cella.network`
- `NetworkMode` -- blocking disposition: `Denylist` (default) or `Allowlist`
- `NetworkRule` -- a domain glob pattern with optional path patterns and an action (block/allow)
- `RuleMatcher` -- evaluates request URLs against compiled rules, returns a `RuleVerdict`
- `ProxyEnvVars` -- resolved proxy environment variables for container injection
- `CaCertificate` -- generated or loaded CA certificate and private key pair
- `MergedNetworkConfig` -- result of merging configs from multiple sources with labeled rules

### Modules

| Module | Purpose |
|--------|---------|
| `config` | Configuration types (`NetworkConfig`, `ProxyConfig`, `NetworkRule`, `NetworkMode`, `RuleAction`), deserialization from TOML and JSON |
| `rules` | Glob-based rule matching engine; `*` matches one segment, `**` matches zero or more (paths only); domain matching is case-insensitive, path matching is case-sensitive |
| `ca` | CA certificate generation via `rcgen`, host CA bundle detection via `rustls-native-certs`, stored at `~/.cella/proxy/` |
| `merge` | Union merge of rules from `devcontainer.json` and `cella.toml`; `cella.toml` wins on per-domain conflicts |
| `proxy_env` | Auto-detection of `HTTP_PROXY`/`HTTPS_PROXY`/`NO_PROXY` from host environment, env pair generation for containers and Docker builds |

## Crate Dependencies

**Depends on:** none (no cella-* dependencies)

**Depended on by:** [cella-agent](../cella-agent), [cella-cli](../cella-cli), [cella-config](../cella-config), [cella-env](../cella-env), [cella-orchestrator](../cella-orchestrator)

## Testing

```sh
cargo test -p cella-network
```

Unit tests cover TOML/JSON deserialization of full and minimal configs, domain glob matching (exact, wildcard subdomain, wildcard middle segment, case insensitivity), path glob matching (`*` single segment, `**` zero-or-more with memoization for pathological patterns), rule evaluation in both denylist and allowlist modes, proxy env var detection and safety entry deduplication, CA certificate generation and reload, and config merge precedence logic.

## Development

Configuration types in `config.rs` deserialize from both TOML (`cella.toml`) and JSON (`customizations.cella.network` in `devcontainer.json`). Changes to these types affect all downstream consumers.

The rule matching engine in `rules.rs` uses memoized recursive matching to handle `**` patterns without exponential blowup. Domain matching uses `*` for single-label wildcards only (no `**` support). Path matching supports both `*` (one segment) and `**` (zero or more segments).

The CA certificate in `ca.rs` is auto-generated on first use and reused on subsequent runs. The private key file is restricted to `0600` permissions on Unix. Host CA bundle detection tries `rustls-native-certs` first, then falls back to well-known filesystem paths across Linux distributions and macOS.
