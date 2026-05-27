# cella-oci

> OCI registry authentication, layer extraction, and cache staging utilities.

Part of the [cella](../../README.md) workspace.

## Overview

cella-oci provides the low-level building blocks for pulling and extracting OCI artifacts from container registries. It handles Docker credential resolution, gzip/tar layer extraction, and atomic cache directory management.

The crate is intentionally minimal — it exposes the plumbing that higher-level crates (`cella-features`, `cella-templates`) compose to fetch devcontainer features and templates from OCI registries.

## Architecture

### Key Types

- `DockerCredentials` — username/password pair resolved from Docker's credential chain
- `RegistryAuth` — authentication token built from resolved credentials (via `oci-distribution`)
- `DEVCONTAINERS_LAYER_MEDIA_TYPE` — media type constant for devcontainer feature layers (`application/vnd.devcontainers.layer.v1+tar`)

### Key Function

```rust
pub fn build_registry_auth(registry: &str) -> RegistryAuth
```

Main entry point for authentication. Resolves credentials for a registry and returns a `RegistryAuth` suitable for passing to `oci-distribution` pull operations.

### Modules

| Module | Purpose |
|--------|---------|
| `auth` | Docker credential resolution from `~/.docker/config.json` — inline `auths`, per-registry `credHelpers`, global `credsStore` (checked in that order) |
| `cache` | Atomic staging/commit for cache directories — `staging_path()` creates a PID-tagged temp path, `commit_staging()` renames atomically with race-safe fallback |
| `extract` | OCI layer extraction — handles gzip tarballs, plain tar, and mismatched media type/magic byte combinations with fallback logic |

## Crate Dependencies

Foundation crate with no cella-* dependencies.

**Depended on by:** [cella-features](../cella-features), [cella-templates](../cella-templates)

## Testing

```sh
cargo test -p cella-oci
```

Unit tests cover base64 auth decoding (valid, empty, missing colon, colons in password), credential resolution chain precedence, staging path generation, atomic commit with race conditions, and layer extraction for gzip tarballs.

One ignored test (`credential_helper_invocation`) requires `docker-credential-desktop` on PATH — run with `cargo test -p cella-oci -- --ignored`.

## Development

The credential resolution chain in `auth.rs` mirrors Docker's own precedence:

1. **Inline `auths`** — base64-encoded `username:password` in `~/.docker/config.json`
2. **`credHelpers`** — per-registry credential helper binaries (`docker-credential-<helper> get`)
3. **`credsStore`** — global credential store binary

The layer extractor in `extract.rs` uses magic byte detection (`0x1f 0x8b` gzip header) as a fallback when the declared media type doesn't match the actual content. This handles real-world registries that mislabel layers.
