# cella-tool-install

> Shared tool installation logic for dev container CLI tools.

Part of the [cella](../../README.md) workspace.

## Overview

cella-tool-install centralizes the install, version-check, and config-mount logic for all supported dev container tools (Claude Code, Codex, Gemini CLI, nvim, tmux). Both `cella up` and `cella install` delegate here so every tool follows the same idempotency, verification, and error-handling patterns.

Each tool installer:
1. Checks if the requested version is already present (short-circuits when it is)
2. Ensures prerequisites (Node.js/npm for npm-based tools, bubblewrap for Codex where the container can run it, Alpine native deps for Claude Code)
3. Runs the installer (curl-based for Claude Code, `npm install -g` for Codex/Gemini, GitHub release download for nvim, system package manager for tmux)
4. Verifies the binary is callable via the same login-shell wrap `cella exec` uses
5. Symlinks into `/usr/local/bin` when the binary is installed somewhere outside the login-shell PATH

Codex ships its own `bwrap` and prefers any `bwrap` it finds on `PATH`, so installing the system package does not add a sandbox — it substitutes a different binary into one Codex runs either way. That substitution is only safe where a new PID namespace can mount a fresh procfs, which Docker's default `MaskedPaths` and `ReadonlyPaths` refuse; elsewhere Codex relies on its own retry that drops `--proc`, and that retry only recognises the wording its bundled build emits, so a distro `bwrap` fails outright instead. cella therefore runs `unshare --user --map-root-user --pid --fork --mount-proc true` as the remote user before assembling the system-package batch, and adds bubblewrap only on exit 0. Where it does not pass, cella installs nothing and Codex uses the `bwrap` it bundles, which starts and enforces with no changes to the container.

This gate only governs what cella installs; it cannot help where the image already ships bubblewrap. The `common-utils` Feature installs it unconditionally, so the dev container base images and anything layered on them arrive with one already present, and cella does not remove packages the image shipped. On those images the post-install probe still reports a sandbox that did not come up.

Once Codex is installed and reachable, `codex sandbox -- /bin/true` runs as the remote user as a safety net, and it is what catches a bubblewrap the image installed rather than cella. It is probed after the `/usr/local/bin` symlink step, since a redirected npm prefix only reaches the login-shell PATH through that symlink. The probe runs against a throwaway `CODEX_HOME` that is removed afterwards, because Codex materializes helper binaries into that directory and `tools.codex.forward_config` bind-mounts the host's `~/.codex` into the container; Codex skips those helpers under a temp home, so what the probe reports is whether the sandbox backend came up, not that a full session would behave identically. A failing probe is a warning, not an install failure. Not every non-zero exit is a degraded sandbox: exit 127 is the shell never finding `codex`, and exit 2 is clap rejecting the arguments — a Codex too old for the `sandbox` subcommand, or a changed CLI surface. Both get their own message, without the bwrap-override remedy that would not apply to either.

`"securityOpt": ["systempaths=unconfined"]` in devcontainer.json lets the procfs mount succeed, so the sandbox gets a private `/proc` instead of the retry's fallback of reusing the container's. It buys that by dropping both of Docker's default sets. `ReadonlyPaths` (`/proc/bus`, `/proc/fs`, `/proc/irq`, `/proc/sys`, `/proc/sysrq-trigger`) become writable, and `/proc/sysrq-trigger` is a host-kernel action primitive — writing to it can reboot, panic, or remount on the host. `MaskedPaths` (`/proc/acpi`, `/proc/asound`, `/proc/interrupts`, `/proc/kcore`, `/proc/keys`, `/proc/latency_stats`, `/proc/sched_debug`, `/proc/scsi`, `/proc/timer_list`, `/proc/timer_stats`, `/sys/firmware`, `/sys/devices/virtual/powercap`, plus one `thermal_throttle` entry per host CPU) become readable.

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
- `tool_config_env_vars()` -- create-time env *pins*: values the agent protocol depends on and a user must not override, currently the container plugin directory, the host home, and the container/host workspace pair used to translate `projectPath`
- `build_tool_config_env_defaults()` -- create-time env *defaults*: knobs owned by the tool itself that a user's `containerEnv` may override, currently `CODEX_SQLITE_HOME`
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
6. Add create-time env in `build_tool_config_env_defaults` (tool-owned, user-overridable) or `tool_config_env_vars` (protocol pins) if the tool needs any
7. Hash any new setting that decides a mount or create-time env into `compute_mount_input_fingerprint` (`cella-compose`), or a change to it will not prompt a rebuild

The `verified_install_step` helper handles post-install verification and PATH remediation -- new tools get this for free by returning their `ExecResult` through the existing branch functions.
