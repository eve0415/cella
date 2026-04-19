# cella-templates

> Dev Container Templates: OCI registry discovery, artifact caching, option validation, and devcontainer.json generation.

Part of the [cella](../../README.md) workspace.

## Overview

cella-templates handles the devcontainer template lifecycle — discovering templates from OCI registries, fetching and caching artifacts, validating options, applying substitutions, and generating devcontainer.json files. It powers `cella init`.

Templates are fetched from OCI registries (default: `ghcr.io/devcontainers/templates`) and cached locally with a 24-hour TTL. The crate also fetches the aggregated devcontainer index from `containers.dev` for template discovery. Option values are validated against the template's metadata (boolean, string with enum constraints, string with proposals). The final output supports both JSONC (with section comments) and plain JSON formats.

## Architecture

### Key Types

- `TemplateMetadata` — parsed `devcontainer-template.json` (id, version, name, description, platforms, options, optionalPaths)
- `InitSelection` — user's choices: template ref, option values, selected features, output format and path
- `SelectedFeature` — a feature reference with its option overrides
- `OutputFormat` — `Jsonc` or `Json`
- `TemplateCache` — file-based cache with 24h TTL, atomic writes via staging paths
- `TemplateOption` — option definition with type, default, proposals (flexible) or enum values (strict)
- `TemplateCollectionIndex` — collection index from an OCI registry
- `DevcontainerIndex` — aggregated index from containers.dev
- `TemplateError` — error enum (registry, fetch, validation, digest mismatch, config exists)

### Modules

| Module | Purpose |
|--------|---------|
| `types` | Core data structures: template metadata, collection indexes, user selections, option definitions |
| `cache` | File-based caching with 24h TTL, atomic directory commits via staging paths |
| `collection` | OCI registry fetching for template and feature collection indexes |
| `fetcher` | Template artifact fetching, tar+gzip extraction, digest validation |
| `index` | Aggregated devcontainer index from containers.dev with offline fallback |
| `apply` | Template application: option substitution, feature merging, JSONC/JSON generation |
| `options` | Option validation and resolution (boolean, string, enum, proposals) |
| `error` | `TemplateError` enum with thiserror |

## Crate Dependencies

**Depends on:** [cella-features](../cella-features), [cella-jsonc](../cella-jsonc)

**Depended on by:** [cella-cli](../cella-cli)

## Testing

```sh
cargo test -p cella-templates
```

Unit tests cover option validation, template substitution, JSONC output formatting, cache operations, and collection parsing. Integration tests (marked with `#[ignore]`) require network access to OCI registries.

## Development

Template resolution follows a cache-first pattern: check local cache, fetch from registry on miss, fall back to stale cache on network failure. Template artifact extraction uses atomic directory commits (write to `<path>.partial-<pid>`, then rename) to prevent corruption. Collection index writes (`put_collection`) use direct `std::fs::write` and are not atomic — a crash during write can leave a truncated index file.

To add a new template source, implement the fetch logic in `collection.rs` or `fetcher.rs` and integrate it with the existing cache layer. The `apply.rs` module handles all post-fetch processing and should not need changes for new sources.
