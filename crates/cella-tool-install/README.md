# cella-tool-install

> Shared tool installation logic for dev container CLI tools.

Part of the [cella](../../README.md) workspace.

## Overview

cella-tool-install centralizes the install, version-check, and config-mount logic for all supported dev container tools (Claude Code, Codex, Gemini CLI, nvim, tmux). Both `cella up` and `cella install` delegate here so every tool follows the same idempotency, verification, and error-handling patterns.

Each tool installer:
1. Checks if the requested version is already present (short-circuits when it is)
2. Ensures prerequisites (Node.js/npm for npm-based tools, bubblewrap for Codex sandbox, Alpine native deps for Claude Code)
3. Runs the installer (curl-based for Claude Code, `npm install -g` for Codex/Gemini, GitHub release download for nvim, system package manager for tmux)
4. Verifies the binary is callable via the same login-shell wrap `cella exec` uses
5. Symlinks into `/usr/local/bin` when the binary is installed somewhere outside the login-shell PATH

Installers return `Option<ExecResult>` -- `None` when the idempotency guard short-circuited, `Some(...)` when the installer ran. Backend errors are flattened into synthetic `ExecResult { exit_code: -1 }` so callers handle all failure modes uniformly.

## Architecture

### Key Types

- `ToolName` -- enum of installable tools (`ClaudeCode`, `Codex`, `Gemini`, `Nvim`, `Tmux`) with config-name, binary-name, and display-name mappings
- `InstallSpec` -- bundles settings, tool list, and probed environment for `install_tools`
- `VerifyOutcome` -- result of checking whether a tool binary is callable (`Reachable`, `InstalledElsewhere`, `NotInstalled`, `ProbeError`)
- `MountSpec` -- bind/tmpfs mount specifications for tool config forwarding (from cella-backend)

### Key Functions

- `install_tools()` -- top-level orchestrator. Runs Claude Code (curl) in parallel with npm tools (sequential to avoid lock contention), nvim, and tmux
- `build_tool_config_mount_specs()` -- produces bind/tmpfs mounts for forwarding host tool configs (~/.claude, ~/.codex, ~/.gemini, ~/.config/nvim, ~/.tmux.conf) into the container
- `ensure_tool_config_paths()` -- pre-creates missing config files/dirs on the host so mount specs can detect them
- `setup_plugin_manifests()` -- populates the tmpfs-backed plugin directory with symlinks and path-rewritten manifest JSONs
- `verify_tool_callable()` -- two-phase probe (login shell, then interactive) matching `cella exec`'s wrapping
- `symlink_to_usr_local_bin()` -- idempotent symlink creation with safety check against overwriting regular files

## Crate Dependencies

**Depends on:** [cella-backend](../cella-backend), [cella-config](../cella-config), [cella-env](../cella-env)

**Depended on by:** [cella-compose](../cella-compose), [cella-orchestrator](../cella-orchestrator)

## Testing

```sh
cargo test -p cella-tool-install
```

Unit tests use a `MockBackend` that replays pre-configured `exec_command` responses in order, covering installer flows, version probing, dependency bootstrapping, config path creation, and verification/symlink logic. All tests are pure (no Docker required).

## Development

When adding a new tool, follow the existing pattern:

1. Add a variant to `ToolName` with config/binary/display name mappings
2. Write an `is_<tool>_installed` check and an `install_<tool>` function returning `Option<ExecResult>`
3. Wire it into `install_tools` (choose the appropriate parallel branch)
4. Add config mount specs in `build_tool_config_mount_specs` if the tool has host config to forward
5. Add host path pre-creation in `ensure_tool_config_paths_in` if needed

The `verified_install_step` helper handles post-install verification and PATH remediation -- new tools get this for free by returning their `ExecResult` through the existing branch functions.
