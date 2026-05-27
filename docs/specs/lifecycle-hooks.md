# Lifecycle Hooks

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT", "SHOULD", "SHOULD NOT", "RECOMMENDED", "MAY", and "OPTIONAL" in this document are to be interpreted as described in [RFC 2119](https://www.ietf.org/rfc/rfc2119.txt).

cella implements the [devcontainer lifecycle hooks specification](https://containers.dev/implementors/spec/) and the [parallel lifecycle script execution proposal](https://github.com/devcontainers/spec/blob/main/docs/specs/parallel-lifecycle-script-execution.md). All behaviors described below apply to cella's implementation of these specs.

## Summary

Devcontainer lifecycle hooks are user-defined commands that execute at specific points during container creation, startup, and attachment. Six phases execute in a fixed order, each supporting three command formats (string, array, object). Phases before the `waitFor` target run in the foreground; phases after it run in the background. Failure in any phase cascades -- all subsequent phases are skipped.

## Lifecycle Phases

Lifecycle commands MUST execute in this strict order:

```
initializeCommand → onCreateCommand → updateContentCommand → postCreateCommand → postStartCommand → postAttachCommand
```

| Phase | Runs on | Trigger | Re-runs on resume? |
|---|---|---|---|
| `initializeCommand` | Host | Before any container operation | Yes |
| `onCreateCommand` | Container | After first container creation | No |
| `updateContentCommand` | Container | After creation or workspace content change | Conditional |
| `postCreateCommand` | Container | After `updateContentCommand` on first creation | No |
| `postStartCommand` | Container | After each container start | Yes |
| `postAttachCommand` | Container | After each tool/editor attachment | Yes |

### `initializeCommand`

`initializeCommand` MUST execute on the host machine, never inside the container. It runs before any container operation begins, including image pulls, builds, and container creation. The container orchestrator MUST be accessible before `initializeCommand` executes. If `initializeCommand` exits non-zero, the pipeline MUST abort immediately -- no container operations occur.

### `onCreateCommand`

`onCreateCommand` MUST run only on first container creation. It MUST NOT re-run on container resume, restart, or subsequent starts. For prebuilt images, `onCreateCommand` runs on first start if it has not yet executed.

### `updateContentCommand`

`updateContentCommand` runs after container creation and MAY re-run when workspace content changes. On creation, it executes after `onCreateCommand`. On subsequent starts, it runs only if the workspace content hash differs from the stored hash.

### `postCreateCommand`

`postCreateCommand` MUST run exactly once after `updateContentCommand` completes during first container creation. It MUST NOT re-run on resume or restart. By default, `postCreateCommand` runs in the background (after the `waitFor` target) unless `waitFor` is set to `postCreateCommand` or later.

### `postStartCommand`

`postStartCommand` MUST execute on every container start, including resume from a stopped state. It is not limited to first creation.

### `postAttachCommand`

`postAttachCommand` MUST execute on every tool/editor attachment event. It MUST NOT execute before the `waitFor` target command completes.

### Phase Behaviors on Resume

On container resume (restarting a stopped container):

- `postStartCommand` and `postAttachCommand` MUST re-run.
- `onCreateCommand` and `postCreateCommand` MUST NOT re-run.
- `updateContentCommand` MUST NOT re-run on resume unless the workspace content hash differs from the stored hash. When the hash differs, it MUST re-run.

### Execution Environment

`initializeCommand` MUST execute on the host machine. All other lifecycle commands (`onCreateCommand` through `postAttachCommand`) MUST execute inside the container as the `remoteUser`, in the configured `workspaceFolder`, with the probed user environment.

When `remoteUser` is set, it MUST override `containerUser` for the user context of spawned lifecycle processes.

All lifecycle commands MUST execute with the current working directory set to the project workspace folder.

## Command Formats

Every lifecycle command property MUST accept exactly three value types: string, array of strings, or object. All six lifecycle command properties MUST share identical format validation rules. Implementations MUST reject any other value type.

### String Format

A string-format lifecycle command MUST be dispatched to a shell interpreter via `/bin/sh -c`. Shell metacharacters (pipes, redirections, variable expansion, globbing) are interpreted by the shell.

```json
{
  "postCreateCommand": "npm install && npm run build"
}
```

String-format commands run sequentially and MUST block until completion before the next phase proceeds.

### Array Format

An array-format lifecycle command MUST execute the command directly without shell interpretation. No metacharacter expansion occurs. The first element is the executable; subsequent elements are arguments. Array items MUST all be strings; non-string elements MUST be rejected.

```json
{
  "postCreateCommand": ["npm", "install", "--prefer-offline"]
}
```

Array-format commands run as a single command and MUST block until completion.

### Object Format (Parallel Execution)

An object-format lifecycle command MUST run each key-value entry in parallel. Keys serve as unique identifiers for each parallel command. Values MUST each be either a string (dispatched via `/bin/sh -c`) or an array of strings (direct exec). String or array values within an object follow the same execution rules as top-level string and array formats.

```json
{
  "postCreateCommand": {
    "server": "npm start",
    "db": ["mysql", "-u", "root", "-p", "my database"]
  }
}
```

All parallel entries MUST succeed (exit code 0) for the phase to succeed. A phase fails if any parallel entry exits non-zero.

### Format Detection

Implementations MUST correctly distinguish string, array, and object forms through value type inspection. All six lifecycle command properties MUST support all three formats consistently.

## Parallel Execution Semantics

Object-format lifecycle commands MUST execute all keyed entries concurrently, not serially. This applies uniformly to all six lifecycle command types.

All parallel commands MUST succeed for the stage to succeed. A lifecycle stage succeeds if and only if every command exits with code 0. If any parallel command fails, the stage MUST be reported as failed.

> **cella extension:** On first failure in a parallel group, cella cancels all remaining in-flight commands (their futures are dropped via `try_join_all`). The failed command's exit code and stderr are reported.

## `waitFor` Semantics

The `waitFor` property controls which lifecycle phases run in the foreground (blocking `cella up` return) versus the background.

`waitFor` MUST default to `updateContentCommand` when not explicitly specified. Scalar properties like `waitFor` MUST use last-value-wins when merged across configuration layers.

`waitFor` accepts one of five lifecycle command names:

| `waitFor` value | Foreground phases | Background phases |
|---|---|---|
| `initializeCommand` | None | All five container phases |
| `onCreateCommand` | `onCreateCommand` | Remaining four |
| `updateContentCommand` (default) | `onCreate`, `updateContent` | Remaining three |
| `postCreateCommand` | First three | Remaining two |
| `postStartCommand` | First four | `postAttachCommand` only |

Phases sequenced after the `waitFor` target run in the background after the tool connection is established. `postAttachCommand` MUST NOT execute before the `waitFor` target command completes.

> **cella extension:** Background phases are spawned via `exec_detached` as a single shell script. On completion, the script writes status to `/tmp/.cella/lifecycle_status.json`:
>
> - Success: `{"status": "completed"}`
> - Failure: `{"status": "failed"}`
>
> The `onCreateCommand` completion state is tracked separately in `/tmp/.cella/lifecycle_state.json` so that prebuilt images can detect whether `onCreateCommand` needs to run on first start.

## Failure Handling

If any lifecycle command exits non-zero, all subsequent lifecycle commands in the sequence MUST be skipped entirely. No further hook commands MAY execute in the current session after an abort.

This cascade applies at the phase level: a failed `onCreateCommand` skips `updateContentCommand`, `postCreateCommand`, `postStartCommand`, and `postAttachCommand`.

For parallel (object-format) commands: if any entry in the parallel group exits non-zero, the entire stage fails and subsequent phases MUST NOT execute.

### Failure Context

> **cella extension:** Failure behavior varies by context:
>
> - **During creation**: The container is stopped, removed, and the error propagates to the caller. No partial container is left behind.
> - **During restart**: The error propagates but the container remains (it was already running before).
> - **Background phases**: Failure is recorded in `/tmp/.cella/lifecycle_status.json`. On the next `cella up`, a warning is displayed with a hint to check `cella logs --lifecycle`.
> - **`initializeCommand` failure**: The pipeline aborts immediately; no container operations occur.

## Feature Lifecycle Commands

Devcontainer features MAY declare their own lifecycle hooks in `devcontainer-feature.json`. These commands are merged into the `devcontainer.metadata` image label during feature resolution.

Feature lifecycle commands MUST follow the same format rules as user-defined lifecycle commands (string, array, or object).

Features MUST NOT contribute `initializeCommand` hooks -- only `onCreateCommand`, `updateContentCommand`, `postCreateCommand`, `postStartCommand`, and `postAttachCommand` are available for feature-contributed lifecycle commands.

Feature lifecycle commands MUST execute in feature installation order (as determined by dependency resolution and `overrideFeatureInstallOrder`). At execution time, lifecycle entries are collected from both the metadata label (feature-contributed) and the user's `devcontainer.json`, then run in order: feature entries first (in install order), user entries last.

Each lifecycle command MUST block until completion before the next command proceeds. This applies to both feature-contributed and user-defined entries within the same phase.

### Image Metadata Label

A single top-level JSON object in the `devcontainer.metadata` label MUST be normalized to a one-element array. When reading lifecycle commands from the metadata label, implementations MUST handle both array (multiple metadata entries) and single-object (one metadata entry) formats.

Feature authors SHOULD copy lifecycle scripts to persistent paths during `install.sh` execution, since the feature build context is not available at runtime.

## Container Labels

### Spec-Standard Labels

Implementations MUST set the following labels on devcontainer-managed containers:

| Label | Value |
|---|---|
| `devcontainer.local_folder` | Canonical host workspace path |
| `devcontainer.config_file` | Canonical config file path |
| `devcontainer.metadata` | JSON array of merged feature + user metadata |

These labels serve as inputs to the `devcontainerId` computation and as container identification markers.

### `devcontainerId` Computation

The `devcontainerId` is a deterministic identifier computed per the [devcontainer spec](https://containers.dev/implementors/spec/):

1. Construct a JSON object from the `idLabels` -- by default, exactly two keys: `devcontainer.local_folder` (canonical workspace path) and `devcontainer.config_file` (canonical config path).
2. Object keys MUST be sorted lexicographically before JSON serialization. The serialized JSON MUST NOT include extraneous whitespace outside key/value strings.
3. The input string MUST be UTF-8 encoded before hashing.
4. Compute the SHA-256 digest of the serialized JSON string.
5. Interpret the SHA-256 hex digest as a big integer (`BigInt`).
6. Convert to base-32 representation.
7. Left-pad the result with `'0'` to exactly 52 characters. The result MUST NOT be truncated.

The output:

- MUST be exactly 52 characters long.
- MUST contain only base-32 alphanumeric characters (digits `0`-`9`, lowercase letters `a`-`v`).
- MUST be deterministic: identical `idLabels` MUST produce identical output.
- MUST be unique among dev containers on the same Docker host (guaranteed by SHA-256 collision resistance).

The `devcontainerId` is used in variable substitution (`${devcontainerId}`) for lifecycle commands, feature mounts, and entrypoints.

### cella-Specific Labels

> **cella extension:** cella sets additional labels beyond the spec-standard set:
>
> | Label | Value |
> |---|---|
> | `dev.cella.tool` | `"cella"` |
> | `dev.cella.workspace_path` | Canonical host workspace path |
> | `dev.cella.config_path` | Canonical config file path |
> | `dev.cella.config_hash` | SHA-256 hex of canonical config JSON |
> | `dev.cella.docker_runtime` | Docker runtime identifier (e.g., `docker-desktop`, `orbstack`) |
> | `dev.cella.created_at` | RFC 3339 timestamp |
> | `dev.cella.backend` | Backend kind (`docker`, `podman`) |
> | `dev.cella.remote_user` | Resolved remote user |
> | `dev.cella.version` | cella version at creation time |
> | `dev.cella.workspace_folder` | Workspace folder path inside container |
> | `dev.cella.remote_env` | JSON array of forwarded env vars |
> | `dev.cella.ports_attributes` | Serialized port attributes |
> | `dev.cella.shutdown_action` | Shutdown behavior (`none`, `stopContainer`, `stopCompose`) |
>
> Compose-managed containers additionally carry `dev.cella.compose_project` and `dev.cella.primary_service`. Worktree containers carry `dev.cella.worktree`, `dev.cella.branch`, and `dev.cella.parent_repo`.

## Variable Substitution

Lifecycle commands support `${devcontainerId}`, `${localWorkspaceFolder}`, `${containerWorkspaceFolder}`, and other spec-defined variables. Substitution MUST be applied after entry resolution but before execution.

## Automatic Configuration

When `postCreateCommand` is not defined and `disableAutomaticConfiguration` is not set to `true`, implementations MAY run automatic setup steps. When `disableAutomaticConfiguration` is `true`, automatic setup MUST be suppressed even when `postCreateCommand` is absent.

## Content Update Detection

> **cella extension:** `updateContentCommand` runs conditionally based on workspace content changes. After lifecycle hooks complete, a content hash of the workspace is written to the container. On subsequent starts, the stored hash is compared against the current workspace; `updateContentCommand` only re-runs when the hash differs.
