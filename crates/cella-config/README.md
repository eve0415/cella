# cella-config

> Devcontainer configuration parsing, validation, and layer merging.

Part of the [cella](../../README.md) workspace.

## Overview

cella-config handles everything related to devcontainer.json: discovering config files, parsing JSONC (JSON with comments), merging configuration layers, validating against the devcontainer schema, and providing type-safe access to all configuration properties.

The crate uses build-time code generation via cella-codegen to produce typed Rust structs directly from the [devcontainer JSON Schema](https://containers.dev/implementors/json_reference/). This ensures that schema changes are automatically reflected in the Rust API without manual synchronization.

Devcontainer configuration is discovered and parsed from the workspace. cella-specific settings (`CellaConfig`) are loaded from up to three layers — global (`~/.cella/config.toml`), `customizations.cella` in devcontainer.json, and project (`.devcontainer/cella.toml`) — merged with deep-merge semantics where later layers override scalars, extend arrays, and recursively merge objects.

### Spec Coverage

Parses all devcontainer.json properties defined in the [Dev Container specification](https://containers.dev/implementors/json_reference/): `image`, `build` (dockerfile, context, args, target), `features`, lifecycle commands (`initializeCommand`, `postCreateCommand`, `postStartCommand`, `postAttachCommand`), `remoteEnv`, `containerEnv`, `mounts`, `forwardPorts`, `portsAttributes`, `remoteUser`, `containerUser`, `customizations`, and more.

## Architecture

### Key Types

- `DevContainer` — generated struct representing a fully parsed devcontainer.json (via `schema` module)
- `CellaSettings` — cella-specific TOML configuration (`~/.cella/config.toml`, `.devcontainer/cella.toml`)
- `Settings` — top-level settings struct with tool-specific config
- `ClaudeCode`, `Codex`, `Gemini` — AI agent tool configuration
- `Tools` — tool detection and forwarding configuration
- `Credentials` — credential forwarding options

### Modules

| Module | Purpose |
|--------|---------|
| `cella_config/cli` | CLI-related config types (`OutputFormat`, `PullPolicy`, `CliBuild`) |
| `cella_config/error` | `CellaConfigError` diagnostics (miette + thiserror) |
| `cella_config/format` | TOML/JSON/JSONC config file loader |
| `cella_config/merge` | Deep-merge logic for config layers (scalars override, arrays extend, maps recurse) |
| `cella_config/security` | `Security` and `SecurityMode` (disabled/logged/enforced) |
| `devcontainer/discover` | Locates devcontainer.json files in workspace directories |
| `devcontainer/parse` | Parses preprocessed JSON (via [cella-jsonc](../cella-jsonc)) into typed config structs |
| `devcontainer/merge` | Merges devcontainer config layers (global -> workspace -> local) |
| `devcontainer/resolve` | Resolves variable references and paths |
| `devcontainer/subst` | Variable substitution (`${localWorkspaceFolder}`, etc.) |
| `devcontainer/diagnostic` | Source-positioned error diagnostics via miette |
| `devcontainer/span` | Byte offset tracking for mapping errors back to source locations |
| `settings/ai_credentials` | AI provider credential forwarding toggles |
| `settings/claude_code` | Claude Code agent configuration |
| `settings/codex` | Codex agent configuration |
| `settings/credentials` | Top-level credential forwarding options (gh, gpg, ssh, ai) |
| `settings/gemini` | Gemini agent configuration |
| `settings/network` | Network settings (proxy, DNS) |
| `settings/nvim` | Neovim forwarding configuration |
| `settings/tmux` | Tmux forwarding configuration |
| `settings/tools` | Tool detection and forwarding configuration |
| `schema` | Auto-generated types from devcontainer JSON Schema (via `build.rs`) |

### Build System

`build.rs` invokes cella-codegen to generate typed Rust structs from the devcontainer JSON Schema. The output is written to `OUT_DIR/generated.rs` and included via `include!()` in the `schema` module.

## Crate Dependencies

**Depends on:** [cella-codegen](../cella-codegen) (build-time), [cella-env](../cella-env), [cella-jsonc](../cella-jsonc), [cella-network](../cella-network)

**Depended on by:** [cella-cli](../cella-cli), [cella-doctor](../cella-doctor), [cella-orchestrator](../cella-orchestrator)

## Testing

```sh
cargo test -p cella-config
```

Tests use snapshot assertions via insta for config parsing and merging. Tempfile-based tests verify config discovery and file I/O. After modifying parsing or codegen output:

```sh
cargo insta review
```

## Development

The JSONC preprocessor (the [cella-jsonc](../cella-jsonc) crate) is a single-pass state machine that preserves byte offsets. This is critical for diagnostics — every parse error can be traced back to the exact position in the original `.jsonc` file. Do not use string replacement on raw JSON as it breaks offset tracking.

When the devcontainer schema changes upstream, update the schema JSON in the build inputs. Codegen will produce new types automatically. Run `cargo insta review` to accept the updated snapshots.

Config merge logic has specific rules for different value types (scalars override, arrays concatenate, maps merge). When adding support for new config properties, ensure the merge behavior matches the devcontainer spec.
