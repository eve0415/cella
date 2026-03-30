# cella-network

- Network proxy configuration, domain/path blocking rules, and CA certificate management
- `config.rs`: Types deserialize from both TOML (`cella.toml`) and JSON (`customizations.cella.network`)
- `rules.rs`: Glob-based matching engine; `*` matches one segment, `**` matches zero or more (paths only)
- Domain matching is case-insensitive; path matching is case-sensitive
- `ca.rs`: Auto-generated CA stored at `~/.cella/proxy/`; uses `rcgen` for X.509 generation
- `merge.rs`: Union merge from devcontainer.json + cella.toml; cella.toml wins on conflict
- `proxy_env.rs`: Auto-detects `HTTP_PROXY`/`HTTPS_PROXY`/`NO_PROXY` from host env, builds container env pairs
