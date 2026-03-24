# cella-features

> Dev Container Features: OCI registry resolution, installation ordering, Dockerfile generation, and caching.

Part of the [cella](../../README.md) workspace.

## Overview

cella-features implements the [Dev Container Features specification](https://containers.dev/implementors/spec/#devcontainer-features). Features are self-contained units of installation logic (e.g., `ghcr.io/devcontainers/features/git:1`) that are layered on top of a base image during container build.

The crate handles the full lifecycle of feature resolution:
1. **Parse** feature references from devcontainer.json (OCI registry refs, local paths, HTTP URLs)
2. **Fetch** feature artifacts from OCI registries (with authentication), local directories, or HTTP endpoints
3. **Read** feature metadata (`devcontainer-feature.json`) for options, dependencies, and lifecycle commands
4. **Order** features by install priority and dependency constraints
5. **Generate** a multi-stage Dockerfile that installs features in the correct order
6. **Cache** fetched artifacts locally for fast rebuilds
7. **Merge** feature configuration (environment variables, mounts, lifecycle commands) back into the devcontainer config

### Spec Coverage

Implements the `features` property from the [Dev Container specification](https://containers.dev/implementors/json_reference/):
- OCI registry resolution (ghcr.io, mcr.microsoft.com, etc.)
- Feature metadata parsing (`devcontainer-feature.json`)
- Option validation and default values
- Install ordering with `installsAfter` dependencies
- Lifecycle command extraction from `devcontainer.metadata` image labels
- Feature environment variables and mounts merging

## Architecture

### Key Types

- `ResolvedFeature` — a fully resolved feature with metadata, options, and artifact directory
- `ResolvedFeatures` — collection of resolved features with merged config
- `FeatureMetadata` — parsed `devcontainer-feature.json` (options, lifecycle, capabilities)
- `FeatureOption` — typed option definition (boolean, string, enum)
- `FeatureCache` — local artifact cache with content-addressed storage
- `Platform` — target platform (os, architecture) for multi-arch resolution
- `FeatureRef` / `NormalizedRef` — parsed and normalized feature references
- `FeatureError` / `FeatureWarning` — error and warning types
- `LifecycleEntry` — lifecycle command from a feature's metadata

### Key Functions

- `resolve_features()` — main entry point: takes devcontainer config, resolves all features, returns merged result
- `lifecycle_from_metadata_label()` — extracts lifecycle commands from `devcontainer.metadata` image labels
- `compute_install_order()` — topological sort of features respecting `installsAfter`
- `generate_dockerfile()` — produces a Dockerfile that installs all features

### Modules

| Module | Purpose |
|--------|---------|
| `reference` | Feature reference parsing and normalization (OCI, local path, HTTP URL) |
| `oci` | OCI registry resolution — pulls feature artifacts from container registries |
| `auth` | OCI registry authentication (token exchange, basic auth) |
| `fetch` | Feature fetching from HTTP endpoints and local directories |
| `metadata` | `devcontainer-feature.json` parsing |
| `ordering` | Install order computation with `installsAfter` dependency resolution |
| `dockerfile` | Dockerfile generation from resolved features (multi-stage build) |
| `cache` | Local artifact caching with content-addressed storage (SHA-256) |
| `merge/feature` | Merges multiple features' configurations together |
| `merge/devcontainer` | Merges feature configuration back into the devcontainer config |
| `merge/image_metadata` | Extracts and merges lifecycle commands from image metadata labels |
| `merge/helpers` | Shared merge utilities (array deduplication, map merging) |
| `merge/validation` | Validates merged configuration for conflicts and missing requirements |
| `types` | Shared type definitions (`ResolvedFeature`, `Platform`, `FeatureMetadata`, etc.) |

## Crate Dependencies

**Depends on:** none (uses oci-distribution, reqwest, bollard, flate2, tar directly)

**Depended on by:** [cella-docker](../cella-docker), [cella-cli](../cella-cli)

## Testing

```sh
cargo test -p cella-features
```

Tests use snapshot assertions via insta for Dockerfile generation and feature ordering. Unit tests with tempfile verify caching and metadata parsing. After modifying Dockerfile generation or ordering:

```sh
cargo insta review
```

## Development

OCI registry interaction is the most complex part of this crate. The flow is: parse the feature reference into a registry/repository/tag tuple, authenticate with the registry, pull the manifest, select the right platform layer, download and extract the tarball.

The cache uses content-addressed storage (SHA-256 of the feature reference) so that identical feature references always hit the cache regardless of when they were fetched. Cache invalidation is manual (delete `~/.cache/cella/features/`).

When adding support for new feature reference formats, start in the `reference` module. The `NormalizedRef` type is what the rest of the pipeline works with — new source types should normalize into it.
