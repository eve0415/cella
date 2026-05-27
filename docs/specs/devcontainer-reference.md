# Devcontainer Reference

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT", "SHOULD", "SHOULD NOT", "RECOMMENDED", "MAY", and "OPTIONAL" in this document are to be interpreted as described in [RFC 2119](https://www.ietf.org/rfc/rfc2119.txt).

cella is a drop-in replacement for the devcontainer CLI. All behaviors documented here match the [official specification](https://containers.dev/implementors/spec/) unless explicitly marked as a **cella extension**.

## Config Discovery

Config discovery searches the workspace root for a devcontainer.json file. The search MUST NOT traverse outside the workspace folder boundary and MUST NOT ascend into parent directories.

### Search Precedence

The implementation MUST search the following locations in order:

| Priority | Path | Behavior |
|---|---|---|
| 1 (highest) | `{workspace}/.devcontainer/devcontainer.json` | If found, discovery stops immediately |
| 2 | `{workspace}/.devcontainer.json` | If found, discovery stops immediately |
| 3 (lowest) | `{workspace}/.devcontainer/<subfolder>/devcontainer.json` | One level deep only |

Reference: [containers.dev/implementors/spec](https://containers.dev/implementors/spec/)

Subfolder scanning at priority 3 MUST be limited to exactly one level deep under `.devcontainer/`. The scan MUST NOT recurse into nested subdirectories within a subfolder.

When configs exist across multiple locations (e.g., both `.devcontainer/devcontainer.json` and `.devcontainer/python/devcontainer.json`), all candidates MUST be discovered and presented for disambiguation. When multiple subfolders at priority 3 each contain a `devcontainer.json`, the implementation MUST fail with an ambiguous configuration error; the user MUST specify `--config-file-path` (or equivalent) to select one.

### Workspace Source Requirement

Configuration validation MUST verify that a workspace source folder is provided before searching. The `devcontainer up` command MUST require a `--workspace-folder` argument pointing to a directory containing a valid devcontainer.json.

### Error Handling

Malformed JSON in devcontainer.json MUST produce a structured parse error, not an unhandled crash or process abort. The parser MUST handle JSONC (JSON with Comments) input, stripping line comments (`//`), block comments (`/* */`), and trailing commas before parsing.

An unterminated block comment MUST produce an error. The devcontainer.json file MUST be valid JSON (after JSONC preprocessing) before configuration processing begins. All devcontainer.json values MUST undergo type validation at the parse boundary.

When no devcontainer.json is found, the behavior is implementation-defined.

### Path Resolution

`localWorkspaceFolder` MUST resolve to the path containing the discovered devcontainer.json (or the folder specified by `--workspace-folder`). When `--config-file-path` is provided, discovery is bypassed entirely and the given path is used directly.

## Orchestration Types

The devcontainer specification defines three mutually exclusive orchestration types. After container type resolution, only the selected type's property set MUST be applied.

Reference: [containers.dev/implementors/json_reference](https://containers.dev/implementors/json_reference/)

### Image-based

Image-based configuration MUST require only the `image` property, referencing a pre-built container image.

```jsonc
{
  "image": "mcr.microsoft.com/devcontainers/base:ubuntu"
}
```

### Dockerfile-based

Dockerfile-based configuration MUST require the `build.dockerfile` property. When the `build` object is present in config, `build.dockerfile` is REQUIRED.

```jsonc
{
  "build": {
    "dockerfile": "Dockerfile",
    "context": ".."
  }
}
```

`build.dockerfile` MUST be resolved relative to the folder containing devcontainer.json, not the workspace root. `build.context` MUST also be resolved relative to the devcontainer.json folder location. Optional build properties include `build.args`, `build.target`, `build.options`, and `build.cacheFrom`.

### Docker Compose-based

Docker Compose-based configuration MUST specify both `dockerComposeFile` and `service`. The `image` and `build.dockerfile` properties are not used because Docker Compose supports them natively within the Compose file.

```jsonc
{
  "dockerComposeFile": "docker-compose.yml",
  "service": "app",
  "workspaceFolder": "/workspaces/project"
}
```

`runServices` MAY specify additional services to start alongside the primary service.

### Mutual Exclusivity

These three types are mutually exclusive. A configuration MUST specify exactly one of: `image`, `build.dockerfile`, or `dockerComposeFile`. Specifying more than one MUST produce a validation error.

### Properties by Orchestration Type

General properties (available to all types): `name`, `forwardPorts`, `portsAttributes`, `otherPortsAttributes`, `containerEnv`, `remoteEnv`, `remoteUser`, `containerUser`, `updateRemoteUserUID`, `userEnvProbe`, `overrideCommand`, `shutdownAction`, `init`, `privileged`, `capAdd`, `securityOpt`, `mounts`, `features`, `overrideFeatureInstallOrder`, `customizations`, `hostRequirements`, and all lifecycle commands.

Image/Dockerfile-specific properties: `image`, `build`, `appPort` (deprecated), `workspaceMount`, `workspaceFolder`, `runArgs`.

Docker Compose-specific properties: `dockerComposeFile`, `service`, `runServices`, `workspaceFolder`.

## Property Merge Semantics

Multiple configuration sources are merged to produce the final resolved configuration. Sources include image metadata labels, feature metadata, and the devcontainer.json file itself.

Reference: [containers.dev/implementors/spec/#merge-logic](https://containers.dev/implementors/spec/#merge-logic)

### Merge Source Order

Merge sources MUST be assembled in deterministic order with devcontainer.json appended last. The merge chain is:

1. Image metadata (lowest precedence)
2. Feature metadata (in installation order)
3. devcontainer.json (highest precedence)

devcontainer.json MUST always be last in the merge source order. Image metadata labels MUST NOT override devcontainer.json for any merge strategy.

### Image Metadata

Container images MAY carry devcontainer configuration in the `devcontainer.metadata` label. The label value MUST be valid JSON -- either a JSON object or a JSON array. When the value is a single JSON object, it MUST be wrapped in an array before merge processing. Image metadata MUST have the lowest precedence in the merge chain.

### Merge Strategies

After merge, every property in the result MUST reflect the correct merge strategy for its type. Property values MUST retain their schema-defined types without implicit coercion.

| Strategy | Properties | Rule |
|---|---|---|
| Boolean OR | `init`, `privileged` | `true` if any source is `true`. MUST only apply to boolean properties. |
| Set union (no duplicates) | `forwardPorts`, `capAdd`, `securityOpt` | Entries from all sources combined; duplicates removed. |
| Concatenate (duplicates allowed) | `mounts`, `runArgs`, `overrideFeatureInstallOrder` | Entries from all sources appended in order. For `mounts`, conflicting mount targets use last-source-wins. |
| Deep merge (per-key) | `features`, `containerEnv`, `remoteEnv`, `customizations`, `portsAttributes`, `otherPortsAttributes` | Recursive key-by-key merge; sibling keys preserved. For `remoteEnv` and `containerEnv`, per-variable last-value-wins. For `portsAttributes`, per-port last-value-wins (not per individual attribute within a port). |
| Max value | `hostRequirements` (per field) | Numeric fields take the maximum across all sources. Memory strings (`"4gb"`, `"512mb"`) are parsed and compared by byte count. Recognized suffixes: `tb`, `gb`, `mb`, `kb` (case-insensitive). Bare numbers are interpreted as bytes. |
| Accumulated (ordered list) | `onCreateCommand`, `updateContentCommand`, `postCreateCommand`, `postStartCommand`, `postAttachCommand`, `initializeCommand` | Commands from all sources accumulate and execute in source order (image metadata first, then features in installation order, then devcontainer.json). |
| Last wins (scalar) | `name`, `image`, `remoteUser`, `containerUser`, `waitFor`, `shutdownAction`, `overrideCommand`, `updateRemoteUserUID`, `userEnvProbe` | Higher-precedence source's value replaces the lower. |

`customizations` merge MUST be delegated to individual tool-specific merge logic for each namespace within the `customizations` object. The outer merge is deep (preserving sibling tool namespaces), but each tool's namespace content is merged according to that tool's own rules.

### Lifecycle Command Accumulation

Lifecycle commands (`onCreateCommand`, `updateContentCommand`, `postCreateCommand`, `postStartCommand`, `postAttachCommand`, `initializeCommand`) accumulate across metadata sources. Commands from image metadata execute first, followed by feature commands in installation order, followed by devcontainer.json commands. Each accumulated command runs to completion before the next begins (within a single source's command; object-format entries within one command still run in parallel).

> **cella extension:** When merging devcontainer.json override layers (global.jsonc, primary config, local override), cella uses last-wins semantics for lifecycle commands within those layers. The accumulation behavior applies to the spec-level merge across image metadata, features, and the final resolved devcontainer.json.

### Command Format Semantics

Lifecycle commands support three formats:

| Format | Execution | Example |
|---|---|---|
| String | MUST be invoked via `/bin/sh -c` with shell metacharacter interpretation | `"npm install && npm start"` |
| Array | MUST bypass the shell entirely; no metacharacter expansion occurs | `["npm", "install", "--production"]` |
| Object | MUST launch all named entries in parallel, not sequentially | `{"install": "npm install", "build": "npm run build"}` |

Each command in an object-format entry MUST exit successfully for the overall stage to be considered successful. Object-form command entry values MUST be either a string (shell-interpreted) or an array of strings (direct execution).

### Default Values

| Property | Default |
|---|---|
| `shutdownAction` | `stopContainer` for image/Dockerfile orchestration; `stopCompose` for Docker Compose |
| `userEnvProbe` | `loginInteractiveShell` |
| `onAutoForward` | `notify` |
| `init` | `false` |
| `privileged` | `false` |
| `overrideCommand` | `true` |
| `updateRemoteUserUID` | `true` |
| `customizations.codespaces.disableAutomaticConfiguration` | `false` (read from devcontainer.json, not image metadata) |

`userEnvProbe` MUST accept exactly the enum values: `none`, `loginShell`, `loginInteractiveShell`, `interactiveShell`. When set to `none`, the implementation MUST skip all environment variable discovery.

`shutdownAction` MUST accept the enum values: `none`, `stopContainer`, `stopCompose`. When `shutdownAction` is `none`, containers MUST be left running on tool disconnect. When `shutdownAction` is `stopContainer`, all containers MUST be stopped when the tool window closes.

### Container User Resolution

`_CONTAINER_USER` MUST be resolved from the correct priority chain of configuration sources (devcontainer.json `containerUser` > image metadata > image default user). When `remoteUser` is not configured, `_REMOTE_USER` MUST equal `_CONTAINER_USER`. Both `_REMOTE_USER` and `_CONTAINER_USER` MUST resolve before feature `install.sh` runs.

## Variable Substitution

Variable substitution resolves `${...}` expressions in configuration string values. Substitution MUST apply only to string-typed property values, not to numbers, booleans, or other JSON types. Object keys MUST NOT be substituted -- only string values within objects and arrays.

Reference: [containers.dev/implementors/json_reference/#variables-in-devcontainerjson](https://containers.dev/implementors/json_reference/#variables-in-devcontainerjson)

### Supported Patterns

| Pattern | Resolution | Scope |
|---|---|---|
| `${localEnv:NAME}` | Host environment variable value | All string properties |
| `${localEnv:NAME:default}` | Host environment variable with fallback | All string properties |
| `${containerEnv:NAME}` | Container environment variable value | `remoteEnv` values only |
| `${containerEnv:NAME:default}` | Container environment variable with fallback | `remoteEnv` values only |
| `${localWorkspaceFolder}` | Canonicalized host workspace path | All string properties |
| `${containerWorkspaceFolder}` | Container workspace path | All string properties |
| `${localWorkspaceFolderBasename}` | Last path component of host workspace | All string properties |
| `${containerWorkspaceFolderBasename}` | Last path component of container workspace | All string properties |
| `${devcontainerId}` | Computed container identifier (52 characters) | All string properties |

### Resolution Rules

Variables MUST be resolved left-to-right in a single pass. Substituted values MUST NOT be rescanned for further variable expressions -- a host variable whose value contains `${localWorkspaceFolder}` MUST produce that literal string, not the workspace path.

`${localEnv:NAME}` MUST resolve from the host environment, not the container. An empty value (`VAR=""`) is considered set -- the default MUST NOT be used. When the variable is unset and no default is provided, the result MUST be an empty string.

`${containerEnv:NAME}` MUST only be valid within the `remoteEnv` property. At config resolution time (before the container is running), `${containerEnv:NAME}` always resolves to the default value (or empty string). The scope restriction prevents cross-context environment variable leakage.

Default values in `${localEnv:NAME:default}` MUST consume everything after the second colon, including additional colons. For example, `${localEnv:UNSET:/usr/bin:/usr/local/bin}` resolves to `/usr/bin:/usr/local/bin`.

Unrecognized variable keywords MUST be passed through verbatim: `${unknownVar:foo}` remains `${unknownVar:foo}`. A `${` without a matching `}` MUST be passed through literally.

When `containerWorkspaceFolder` is not explicitly set, it MUST default to `/workspaces/<localWorkspaceFolderBasename>`.

### Deferred Evaluation

Variable substitution MUST be deferred to apply time, not performed during merge. This ensures that variables from different sources resolve against the correct runtime context rather than being prematurely substituted before merge processing.

`workspaceFolder` MUST be resolved in a first pass so that `${containerWorkspaceFolder}` references in other fields use the substituted value:

```jsonc
{
  "workspaceFolder": "/workspaces/${localWorkspaceFolderBasename}",
  "mounts": ["source=data,target=${containerWorkspaceFolder}/data,type=volume"]
}
```

The mount target resolves to `/workspaces/<basename>/data`, using the already-substituted `workspaceFolder`.

After variable substitution completes, no unresolved `${var}` references SHOULD remain in string values (excluding unrecognized keywords, which are passed through).

### Template Placeholders

Template files use a separate substitution syntax: `${templateOption:optionId}`. Template substitution MUST apply to all files in the template, not just devcontainer.json. After template application, no unresolved `${templateOption:*}` placeholders MUST remain in any file.

### Application Timing

Properties have strict timing requirements for when their values are applied:

**At container creation (Docker layer):**

- `containerEnv` MUST be applied at container creation time as environment variables baked into the container. `containerEnv` MUST be set as Dockerfile ENV commands before feature `install.sh` runs.
- `containerUser` MUST be applied at container creation time.
- Container mounts MUST be applied at container creation time.

`remoteUser` and `remoteEnv` MUST NOT be applied during the container creation stage. This separation enables the image `ENTRYPOINT` to execute with different permissions than the developer.

**Post-creation (runtime layer):**

- `remoteEnv` and `remoteUser` MUST be applied to all processes created in the post-creation stage, including lifecycle commands (`postCreateCommand`, `postStartCommand`, `postAttachCommand`) and terminal sessions.
- `remoteEnv` and `remoteUser` MUST NOT be applied until the `waitFor` gate is reached.

**On resume:**

- `remoteEnv` and `remoteUser` MUST be reapplied to all processes on environment resume, before executing lifecycle commands (`postStartCommand`, `postAttachCommand`).

After env probe completes, `remoteEnv` overrides MUST be applied on top of probed values. A `remoteEnv` entry with a `null` value MUST unset that environment variable in the container.

## Customizations

The `customizations` property provides a namespace for tool-specific configuration. Each tool or service reads only its own namespace from the customizations object.

Reference: [containers.dev/implementors/json_reference/#customizations](https://containers.dev/implementors/json_reference/#customizations)

### Namespace Contract

Customization properties MUST be namespaced per tool under the `customizations` key. Tool-specific customizations MUST use vendor-namespaced key prefixes per the specification.

```jsonc
{
  "customizations": {
    "vscode": {
      "extensions": ["ms-python.python"],
      "settings": { "python.defaultInterpreterPath": "/usr/bin/python3" }
    },
    "cella": {
      "credentials": { "gh": true }
    }
  }
}
```

Each tool/service MUST read only its own namespace from the `customizations` object. Non-namespaced customization keys MUST NOT shadow or override built-in devcontainer properties. Customization namespace keys are opaque, tool-defined strings assumed to be globally unique across all registered tools.

### Tool-specific Dispatch

After merge, only the target tool's customization namespace MUST be applied. Merging of customization content is delegated to each tool's own logic -- the devcontainer specification does not prescribe how individual tools merge their namespace content.

### VS Code Customizations

VS Code customizations (`customizations.vscode`) apply to both the Dev Containers extension and GitHub Codespaces. The `extensions` array MUST accept both install and removal directives in the same list. The `@prerelease` suffix in an extension ID MUST be treated as a channel tag, not a semver prerelease version.

### Codespaces Customizations

`customizations.codespaces` accepts `repositories`, `openFiles`, and `disableAutomaticConfiguration`. `disableAutomaticConfiguration` defaults to `false` when the property is absent.

## devcontainerId Computation

The `devcontainerId` is a deterministic 52-character identifier computed from the workspace and config file paths. It MUST be stable across container rebuilds with the same configuration and MUST differ across different workspaces or config file locations.

Reference: [containers.dev/implementors/spec/#devcontainerid](https://containers.dev/implementors/spec/#devcontainerid)

### Algorithm

1. **Construct input object.** Create a JSON object with two keys:
   - `devcontainer.config_file` -- the canonicalized absolute path to the devcontainer.json file
   - `devcontainer.local_folder` -- the canonicalized absolute path to the workspace root

2. **Sort keys.** Object keys MUST be sorted lexicographically before serialization. Using a `BTreeMap` (or equivalent sorted map) satisfies this requirement.

3. **Serialize.** JSON serialization MUST NOT include optional whitespace outside key/value strings. The output is compact JSON (`JSON.stringify` with no whitespace arguments, or `serde_json::to_string`).

4. **Hash.** The input string MUST be UTF-8 encoded before computing the SHA-256 digest.

5. **Convert to base-32.** The SHA-256 digest MUST be interpreted as a big integer (big-endian byte order) and converted to base-32 using the digit set `0-9` and `a-v` (32 characters total).

6. **Pad.** The base-32 string MUST be left-padded with `'0'` to reach exactly 52 characters. The result MUST never be truncated.

### Output Constraints

- The devcontainerId MUST be exactly 52 characters long.
- The devcontainerId MUST contain only base-32 alphanumeric characters: digits `0-9` and lowercase letters `a-v`.
- The devcontainerId MUST be stable (identical output) across rebuilds of the same configuration.
- SHA-256 collision resistance provides practical uniqueness guarantees.

### Example

For a workspace at `/home/user/projects/myapp` with config at `/home/user/projects/myapp/.devcontainer/devcontainer.json`:

1. Input JSON (keys sorted): `{"devcontainer.config_file":"/home/user/projects/myapp/.devcontainer/devcontainer.json","devcontainer.local_folder":"/home/user/projects/myapp"}`
2. SHA-256 hash of the UTF-8 bytes
3. Interpret hash as big integer, convert to base-32
4. Left-pad to 52 characters

## Lockfile Integrity

> This section describes behavior beyond the official devcontainer specification.

Feature and template lockfiles provide supply-chain integrity verification. The lockfile integrity value MUST be the SHA-256 hash of the downloaded artifact bytes. On first use with no existing lockfile entry (trust-on-first-use), the artifact integrity MUST be recorded before installation. After recording, the lockfile entry MUST contain non-null `integrity` and `version` fields.

## Secrets

Secrets MUST NOT be stored directly in devcontainer.json. devcontainer.json from shared repositories MUST be treated as potentially attacker-controlled input. Credentials MUST NOT appear in log output, error messages, or diagnostic output. After config reload, cached or previously validated secrets MUST be re-validated.

Secret injection into containers MUST use `remoteEnv`-equivalent semantics (applied post-creation, not baked into the container image layer). See [credential-protection.md](credential-protection.md) for the full phantom token system.
