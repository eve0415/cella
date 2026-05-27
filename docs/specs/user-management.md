# User Management

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT", "SHOULD", "SHOULD NOT", "RECOMMENDED", "MAY", and "OPTIONAL" in this document are to be interpreted as described in [RFC 2119](https://www.ietf.org/rfc/rfc2119.txt).

cella is a drop-in replacement for the devcontainer CLI. All user management behavior MUST match the [devcontainer JSON reference](https://containers.dev/implementors/json_reference/) for `remoteUser`, `containerUser`, and `updateRemoteUserUID`.

## Summary

Dev containers distinguish between two user contexts: `containerUser` for all container-level operations and `remoteUser` for tools and terminal sessions. UID/GID remapping ensures bind-mounted files have correct ownership on Linux hosts.

## User Types

### containerUser

The `containerUser` property sets the user for all operations executed inside the container. This affects the user context for lifecycle hooks, Feature installation scripts, and any process the container runtime starts.

| Property | Type | Default |
|---|---|---|
| `containerUser` | `string` | Image's `USER` instruction (typically `root`) |

### remoteUser

The `remoteUser` property sets the user for tools, terminals, tasks, and debugging sessions connected to the container. It does NOT change the container's overall runtime user.

| Property | Type | Default |
|---|---|---|
| `remoteUser` | `string` | Value of `containerUser` |

`remoteUser` MUST default to the value of `containerUser` when not explicitly set. When both are set, `remoteUser` overrides `containerUser` for spawned process user context (terminals, tasks, debugging).

### Distinction

- `containerUser` affects ALL container operations.
- `remoteUser` affects ONLY tool and terminal processes.
- Setting `remoteUser` without `containerUser` runs the container as the image default user but opens terminals as `remoteUser`.

## Application Timing

- `containerUser` MUST be applied at container creation time. It is baked into the container configuration and cannot be changed after creation.
- `remoteUser` MUST be applied post-creation only. It affects processes spawned after the container is running (terminals, exec sessions, tasks).

## UID/GID Remapping

### updateRemoteUserUID

| Property | Type | Default |
|---|---|---|
| `updateRemoteUserUID` | `boolean` | `true` |

When `true`, the container runtime updates the UID and GID of `remoteUser` (or `containerUser` if `remoteUser` is not set) to match the local host user's UID and GID. This prevents permission problems with bind mounts.

### Platform Behavior

- `updateRemoteUserUID` MUST only apply on Linux hosts.
- On macOS and Windows, `updateRemoteUserUID` MUST be ignored regardless of its value.

### Bind Mount Context

UID/GID remapping is primarily relevant when using bind mounts. Without remapping, files created inside the container may have different ownership than the host user, causing permission conflicts when accessing the workspace from the host.

## Defaults

The default resolution chain:

1. `containerUser` defaults to the image's `USER` instruction (or `root` if none).
2. `remoteUser` defaults to `containerUser`.
3. `updateRemoteUserUID` defaults to `true`.

Configurations inherit `remoteUser` from base image metadata when using the [image metadata](image-metadata.md) merge system. In the merge, `remoteUser` and `containerUser` both use last-value-wins semantics -- the devcontainer.json value takes precedence over image metadata.
