# Image Metadata

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT", "SHOULD", "SHOULD NOT", "RECOMMENDED", "MAY", and "OPTIONAL" in this document are to be interpreted as described in [RFC 2119](https://www.ietf.org/rfc/rfc2119.txt).

cella is a drop-in replacement for the devcontainer CLI. All image metadata behavior MUST match the [devcontainer image metadata specification](https://github.com/devcontainers/spec/blob/main/docs/specs/image-metadata.md).

## Summary

Dev container configuration and Feature metadata can be embedded in prebuilt images via the `devcontainer.metadata` label. This allows images to carry their configuration so that a consuming `devcontainer.json` does not need to repeat it. At runtime, metadata from the image, installed Features, and the user's `devcontainer.json` are merged using property-specific rules.

## Image Metadata Label

Dev container metadata is stored as a Docker image label named `devcontainer.metadata`. The label value is a JSON string representing configuration entries.

### Format

The label value MUST be a JSON array of objects, where each object contains dev container configuration properties:

```json
[
  {
    "remoteUser": "vscode",
    "customizations": { "vscode": { "extensions": ["ms-python.python"] } }
  },
  {
    "id": "ghcr.io/devcontainers/features/node:1",
    "init": true,
    "customizations": { "vscode": { "extensions": ["dbaeumer.vscode-eslint"] } }
  }
]
```

Entries without an `id` field represent base image or devcontainer.json configuration. Entries with an `id` field represent installed Feature metadata.

## Label Parsing

- The label value MUST be parsed as JSON.
- A JSON array is the expected format.
- A single top-level object (not wrapped in an array) MUST be accepted and treated as a single-element array.
- Malformed JSON in the `devcontainer.metadata` label MUST produce a parse error, not a crash.

## Merge Precedence

When applying metadata at runtime, three sources are merged in order of increasing precedence:

1. **Image metadata** (lowest) -- from the `devcontainer.metadata` label on the base image
2. **Feature metadata** -- from Features installed on top of the image
3. **devcontainer.json** (highest) -- the user's configuration file

The devcontainer.json is always considered last and its values take highest precedence.

### Per-Property Merge Logic

| Property | Type | Merge Rule |
|---|---|---|
| `init` | `boolean` | `true` if at least one source is `true` |
| `privileged` | `boolean` | `true` if at least one source is `true` |
| `capAdd` | `string[]` | Union of all arrays, no duplicates |
| `securityOpt` | `string[]` | Union of all arrays, no duplicates |
| `entrypoint` | `string` | Collected list of all entrypoints |
| `mounts` | `array` | Collected list of all mounts; on conflict, last source wins |
| `onCreateCommand` | `string \| string[]` | Collected list of all commands |
| `updateContentCommand` | `string \| string[]` | Collected list of all commands |
| `postCreateCommand` | `string \| string[]` | Collected list of all commands |
| `postStartCommand` | `string \| string[]` | Collected list of all commands |
| `postAttachCommand` | `string \| string[]` | Collected list of all commands |
| `waitFor` | `enum` | Last value wins |
| `customizations` | `object` | Merging left to individual tools |
| `containerUser` | `string` | Last value wins |
| `remoteUser` | `string` | Last value wins |
| `userEnvProbe` | `string` | Last value wins |
| `remoteEnv` | `object` | Per variable, last value wins |
| `containerEnv` | `object` | Per variable, last value wins |
| `overrideCommand` | `boolean` | Last value wins |
| `portsAttributes` | `map` | Per port, last value wins |
| `otherPortsAttributes` | `object` | Last value wins |
| `forwardPorts` | `array` | Union, no duplicates; mapping changes use last value |
| `shutdownAction` | `enum` | Last value wins |
| `updateRemoteUserUID` | `boolean` | Last value wins |
| `hostRequirements` | `object` | Per field, max value wins |

Variables in string values are substituted at the time the value is applied.

## Build Options

All build-related properties MUST be forwarded to the underlying `docker build` command.

### Required Forwarding

The following `build` sub-properties MUST all be passed through to docker build:

| Property | Type | Description |
|---|---|---|
| `context` | `string` | Build context path. MUST be restricted to the project directory. |
| `args` | `object` | Build arguments. Values MUST be strings, not other JSON types. |
| `target` | `string` | Multi-stage build target |
| `cacheFrom` | `string \| string[]` | Cache sources. Accepts either a single string or an array of strings. |
| `options` | `string[]` | Additional arguments passed directly to docker build |

### Build Argument Handling

- Build `options` MUST be appended after all other build arguments.
- Build `options` array ordering MUST be preserved in the final command.
- An empty or null `build.options` array MUST be handled without build failure.
- After build options are appended, base security arguments MUST NOT be overridden by user-provided options.

### Deprecated Properties

- The deprecated `dockerFile` property MUST be mapped to `build.dockerfile` before build processing.
- The modern Dockerfile form requires `build.dockerfile` to be set.

### Image Property

- The `imageContainer` scenario requires the `image` property to be a non-empty string.

### Feature Installation

- `install.sh` MUST always execute as root during the image build phase.

## Metadata Embedding

After building an image, the `devcontainer.metadata` label MUST contain the merged configuration from all sources (base image metadata, Feature metadata, and devcontainer.json properties).

### Cache Invalidation

- Cached devcontainer IDs MUST be invalidated when `idLabels` change.

### OCI Compliance

- Referrers API responses MUST use the content type `application/vnd.oci.image.index.v1+json`.
