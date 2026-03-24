# cella-env

> Environment forwarding orchestration for dev containers.

Part of the [cella](../../README.md) workspace.

## Overview

cella-env detects the host environment (SSH agent, git config, credential proxies, AI agent tools) and produces the mounts, environment variables, and post-start commands needed to forward that environment into a dev container. It is the single entry point for all environment setup during `cella up`.

The crate is designed to never fail. Each forwarding feature (SSH agent, git config, credentials) is independently detected and configured. If any feature's detection fails, it logs a warning and is skipped — the container still starts. This follows the principle that `cella up` should always succeed even if some environment forwarding isn't available.

Environment forwarding happens in two phases:
- **Phase A (container creation):** Bind mounts and environment variables that must be set before the container starts
- **Phase B (post-start injection):** File uploads (SSH config, credential helper scripts) and git config commands that run after the container starts and UID remapping completes

### Spec Coverage

Implements the environment-related portions of the [Dev Container specification](https://containers.dev/implementors/json_reference/): `remoteEnv`, `containerEnv`. Also implements VS Code-equivalent behavior for SSH agent forwarding and git credential forwarding that the official CLI lacks.

## Architecture

### Key Types

- `EnvForwarding` — complete forwarding result: mounts, env vars, and post-start injection
- `PostStartInjection` — files to upload and git config commands to run after container start
- `ForwardMount` — a bind mount (source path on host, target path in container)
- `ForwardEnv` — an environment variable (key, value)
- `FileUpload` — file content to upload into the container (path, content, permissions)
- `DockerRuntime` — detected container runtime (DockerDesktop, OrbStack, LinuxNative, Colima, Unknown)
- `GitConfigEntry` — a git config key-value pair to forward

### Key Function

```rust
pub fn prepare_env_forwarding(config: &Value, remote_user: &str) -> EnvForwarding
```

This is the main entry point. It detects the runtime, probes the host environment, and assembles the complete forwarding configuration.

### Modules

| Module | Purpose |
|--------|---------|
| `platform` | Runtime detection from `DOCKER_HOST`, `DOCKER_CONTEXT`, and `docker context inspect` |
| `ssh_agent` | SSH agent socket detection and mount configuration (platform-aware) |
| `ssh_config` | SSH config file reading (`~/.ssh/config`, `~/.ssh/known_hosts`) for upload into container |
| `git_config` | Host git config reading (safe subset of `user.name`, `user.email`, etc.) |
| `git_credential` | Credential proxy socket/TCP detection and helper script generation |
| `gh_credential` | gh CLI credential forwarding (auto-on when gh is installed) |
| `user_env_probe` | Host environment variable probing for `userEnvProbe` spec support |
| `claude_code` | Claude Code config detection, path rewriting, and container injection |
| `codex` | OpenAI Codex CLI host detection and container path helpers |
| `gemini` | Google Gemini CLI host detection and container path helpers |

## Crate Dependencies

**Depends on:** [cella-config](../cella-config)

**Depended on by:** [cella-cli](../cella-cli), [cella-doctor](../cella-doctor)

## Testing

```sh
cargo test -p cella-env
```

Unit tests cover runtime detection logic, git config parsing, and environment variable assembly.

## Development

The runtime detection in `platform.rs` determines which credential forwarding transport to use:
- **Socket-based** (Linux native, Docker Desktop): bind-mount the credential proxy socket directly
- **TCP-based** (OrbStack, Colima): use `host.docker.internal:<port>` since socket bind-mounting doesn't work across VMs

When adding a new environment forwarding feature, follow the existing pattern: detect on the host, produce `ForwardMount`/`ForwardEnv`/`FileUpload` entries, and handle detection failures gracefully with `tracing::warn`.
