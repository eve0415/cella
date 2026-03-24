# cella

*Latin: "small room, chamber"*

A terminal-native devcontainer CLI. Built for agents.

[![License: GPL-3.0](https://img.shields.io/badge/license-GPL--3.0-blue.svg)](LICENSE)
[![Language: Rust](https://img.shields.io/badge/language-Rust-orange.svg)](https://www.rust-lang.org/)
[![Status: Alpha](https://img.shields.io/badge/status-alpha-yellow.svg)](#)
[![GitHub stars](https://img.shields.io/github/stars/eve0415/cella?style=social)](https://github.com/eve0415/cella)

> [!WARNING]
> cella is in early development. Core commands work, but expect breaking changes.

## Why

**The spec is great. The tooling isn't.** The [Dev Container specification](https://containers.dev/) ([spec](https://github.com/devcontainers/spec)) has become a de facto standard — adopted by VS Code, JetBrains, GitHub Codespaces, CodeSandbox, and others. But the spec defines features like `forwardPorts` and `portsAttributes` that the [reference CLI](https://github.com/devcontainers/cli) simply doesn't implement. SSH agent forwarding, credential forwarding, port forwarding, and BROWSER interception all require the [VS Code Dev Containers extension](https://marketplace.visualstudio.com/items?itemName=ms-vscode-remote.remote-containers) (which runs vscode-server inside the container) to work. The CLI is a CI build tool, not a general-purpose devcontainer runtime.

**Agents are the first developers now.** AI coding agents — Claude Code, Codex, Gemini CLI — are terminal-based. They're the primary developers on many projects today. Dev containers should integrate with the terminal, not require a GUI editor. Use your own tools — Ghostty, WezTerm, Windows Terminal, tmux, Neovim — or just let the agent work headless.

**VS Code is becoming Copilot-first.** Every update pushes Copilot deeper — the sidebar opens on every new directory, features compete for attention with AI integrations. If you're not using Copilot, you're paying the overhead anyway. Dev containers shouldn't require VS Code for basic functionality like port forwarding.

**Agents pollute workspaces.** AI agents treat `/tmp` as free storage — throwaway scripts, diff files, temp artifacts, no cleanup. Containers are the answer: destroy and rebuild clean. One branch, one container, clean slate.

**The official CLI has real gaps.** It requires Node.js to run (ironic for a container tool). The maintainers confirmed: SSH agent forwarding is ["part of the extension, not the CLI"](https://github.com/devcontainers/cli/issues/441). Port forwarding was left out because ["it requires NodeJS inside the container"](https://github.com/devcontainers/cli/issues/22). No `stop` or `down` command exists ([cli#386](https://github.com/devcontainers/cli/issues/386)). cella fixes all of this with a single native binary.

## Quick Start

Requires a [Rust toolchain](https://rustup.rs/) and a Docker API-compatible runtime (Docker Engine, OrbStack, Podman, Colima, etc.).

```sh
# Build from source (no published binaries yet)
cargo build --release
# Binary at target/release/cella

# Start a dev container
cella up

# Open a shell inside the container
cella shell

# Run a command
cella exec cargo test

# Stop and remove the container
cella down
```

## cella vs @devcontainers/cli

The [Dev Container specification](https://containers.dev/) ([spec repo](https://github.com/devcontainers/spec)) defines the standard. The [@devcontainers/cli](https://github.com/devcontainers/cli) is the official reference implementation, but many spec-defined features only work with the [VS Code extension](https://marketplace.visualstudio.com/items?itemName=ms-vscode-remote.remote-containers) (which runs vscode-server inside the container).

| | cella | @devcontainers/cli |
|---|---|---|
| Language | Rust (single native binary) | TypeScript (requires Node.js) |
| Runtime dependency | None | Node.js 14+ |
| `stop` / `down` command | Yes | No ([cli#386](https://github.com/devcontainers/cli/issues/386)) |
| Port forwarding | Automatic (daemon + in-container agent) | No — [VS Code extension only](https://github.com/devcontainers/cli/issues/22) ([cli#186](https://github.com/devcontainers/cli/issues/186)) |
| SSH agent forwarding | Platform-aware (Docker Desktop, OrbStack, Linux) | No — [VS Code extension only](https://github.com/devcontainers/cli/issues/441) |
| Git credential forwarding | gh CLI via socket + TCP, auto-on | No — [VS Code extension only](https://github.com/microsoft/vscode-remote-release/issues/4202) |
| BROWSER interception | Host browser opens for OAuth | No — [VS Code extension only](https://github.com/microsoft/vscode-remote-release/issues/9935) |
| Container listing | `cella list` | No ([cli#843](https://github.com/devcontainers/cli/issues/843)) |
| Config validation | Source-positioned diagnostics | Basic |
| Docker Compose | Not yet | Yes |
| Podman | Not yet | Yes |
| Editor requirement | None (any terminal) | VS Code for full feature set |

## Features

### Container Lifecycle

- [x] `cella up` / `cella down` — start and stop containers
- [x] `cella shell` — attach to container shell
- [x] `cella exec` — run commands (interactive and detached)
- [x] `cella build` — pre-build images
- [x] `cella list` — list containers with status and ports
- [x] Devcontainer Features (OCI registry resolution, install ordering, caching)
- [x] Lifecycle commands (postCreate, postStart, postAttach)
- [x] Image and Dockerfile builds
- [x] Config validation with source-positioned diagnostics
- [ ] Docker Compose support
- [ ] Container logs streaming
- [ ] Diagnostics (`cella doctor`)

### Environment & Credentials

- [x] SSH agent forwarding (Docker Desktop, OrbStack, Linux)
- [x] Git config forwarding
- [x] gh CLI credential forwarding (auto-on)
- [x] Environment variable forwarding (remoteEnv, containerEnv)
- [x] User environment probing

### Port Forwarding

- [x] Automatic port detection via /proc/net/tcp
- [x] Host daemon + in-container agent
- [x] BROWSER interception (OAuth callbacks)
- [x] OrbStack-aware port handling

### Runtime Support

- [x] Docker Engine
- [x] OrbStack
- [ ] Podman
- [ ] Colima / Lima

### Planned

- [ ] Git worktree integration (1 branch = 1 container)
- [ ] AI agent launching (`cella branch --agent`)
- [ ] tmux integration
- [ ] Neovim integration
- [ ] Templates & global config
- [ ] Project initialization (`cella init`)

## Architecture

cella is a Rust workspace with 11 focused crates. The CLI delegates all business logic to library crates — no logic lives in the binary entry point.

```
┌─────────────────────────────────────────────────┐
│                   cella-cli                     │
│         (command parsing, user output)          │
├──────────┬──────────┬──────────┬────────────────┤
│cella-git │cella-dock│cella-port│  cella-agent   │
│(worktree)│(container│(port     │  (in-container │
│          │ runtime) │ mgmt)    │   agent)       │
├──────────┴──────────┴──────────┴────────────────┤
│                 cella-config                    │
│        (devcontainer.json, JSONC, TOML)         │
└─────────────────────────────────────────────────┘
```

See [docs/architecture.md](docs/architecture.md) for details.

## Contributing

Contributions welcome. See the [contributing guide](docs/contributing.md) for build instructions and code style.

- Questions and ideas: [GitHub Discussions](https://github.com/eve0415/cella/discussions)
- Bug reports: [GitHub Issues](https://github.com/eve0415/cella/issues)

---

If you find cella useful, consider giving it a star on [GitHub](https://github.com/eve0415/cella). It helps others discover the project.

## License

[GPL-3.0](LICENSE)
