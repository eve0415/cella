# Getting Started

Cella is a terminal-native devcontainer CLI built in Rust. It's a drop-in replacement for the [@devcontainers/cli](https://github.com/devcontainers/cli) that works without VS Code -- port forwarding, SSH agent forwarding, credential forwarding, and clipboard integration all work from any terminal. Single binary, no Node.js runtime, designed for AI coding agents and humans alike.

## Installation

```sh
brew install eve0415/tap/cella                                        # Homebrew
curl -fsSL https://raw.githubusercontent.com/eve0415/cella/main/install.sh | sh  # install script
cargo install --git https://github.com/eve0415/cella cella-cli        # from source (Rust 1.95+)
```

Pre-built binaries for macOS and Linux are also available on [GitHub Releases](https://github.com/eve0415/cella/releases).

## Quick Start

You need a Docker-compatible runtime ([Docker Engine](https://docs.docker.com/engine/install/), [OrbStack](https://orbstack.dev/), or [Colima](https://github.com/abiosoft/colima)). Run `cella doctor` to check your setup.

If your project doesn't have a `.devcontainer/devcontainer.json` yet, run `cella init` to create one interactively.

### Start the container

```sh
cella up
```

Cella builds the image (if needed), starts the container, runs lifecycle commands, and forwards your SSH agent, git config, and credentials automatically.

### Work inside the container

```sh
cella shell              # open a shell
cella exec cargo test    # run a command
cella exec -- npm start  # use -- for commands with flags
```

### Stop when you're done

```sh
cella down               # stop the container
cella down --rm          # stop and remove it
```

## Key Features

### Environment forwarding

Cella automatically forwards your host environment into containers -- no manual configuration needed:

- **SSH agent** -- works across Docker Desktop, OrbStack, Colima, and native Linux
- **Git config** -- your name, email, and aliases carry over
- **GitHub CLI** -- `gh` credentials are forwarded so authentication works inside containers
- **Clipboard** -- bidirectional forwarding via xsel/xclip

### AI tool integration

Install AI coding tools into running containers and forward their host config automatically:

```sh
cella install claude-code    # install Claude Code (mounts ~/.claude/)
cella install codex          # install Codex CLI (mounts ~/.codex/)
cella install --all          # install everything
```

AI provider API keys (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, etc.) are read live from the host on every `exec` or `shell` -- never baked into the image. Control which providers are forwarded in [cella.toml](configuration.md).

### Port forwarding

Ports are detected and forwarded automatically. Each container gets hostname-based URLs:

```
http://3000.main.myapp.localhost
http://8080.feature-auth.myapp.localhost
```

```sh
cella ports         # see forwarded ports
cella ports --all   # across all worktree containers
```

See the [port forwarding guide](port-forwarding.md) for dev server configuration and OrbStack details.

### Credential protection

Phantom tokens prevent real API keys from ever entering container memory. Instead of forwarding credentials as environment variables, cella injects opaque placeholders that are resolved by the host daemon at request time.

```toml
[credentials]
protect = true
```

See the [credential protection guide](credential-protection.md) for the full threat model and configuration.

### Worktree-based parallel development

Every git branch gets its own isolated container:

```sh
cella branch fix/login-bug --base main    # create branch + container
cella switch fix/login-bug                # open shell in that container
cella list                                # see all containers
cella prune                               # clean up merged branches
```

Run background tasks across containers:

```sh
container$ cella task run fix/login-bug -- cargo test
container$ cella task logs -f fix/login-bug
```

See the [worktree guide](worktrees.md) for the full workflow and in-container commands.

### Network proxy

Block or allow network traffic at the domain and path level with a transparent proxy:

```toml
[network]
mode = "denylist"

[[network.rules]]
domain = "*.production.example.com"
action = "block"
```

See the [network proxy guide](network-proxy.md) for modes, path patterns, and CA certificate handling.

## Configuration

Cella uses a layered config system:

| Priority | Location | Scope |
|----------|----------|-------|
| 1 (lowest) | `~/.cella/config.toml` | Global defaults |
| 2 | `customizations.cella` in `devcontainer.json` | Shared with version control |
| 3 (highest) | `.devcontainer/cella.toml` | Project-level overrides |

Example project config (`.devcontainer/cella.toml`):

```toml
[credentials]
gh = true

[tools]
install = ["claude-code", "nvim"]
```

See the [configuration guide](configuration.md) for all settings and merge semantics.

## Common Commands

| Command | Description |
|---------|-------------|
| `cella up` | Start a dev container |
| `cella down` | Stop (and optionally remove) the container |
| `cella shell` | Open a shell inside the container |
| `cella exec <cmd>` | Run a command in the container |
| `cella build` | Build the image without starting |
| `cella list` | List all containers with status |
| `cella logs` | View container logs |
| `cella ports` | Show forwarded ports |
| `cella branch <name>` | Create a worktree branch with its own container |
| `cella switch [name]` | Switch to a worktree container (fuzzy picker if omitted) |
| `cella prune` | Remove merged worktrees and their containers |
| `cella install [tools]` | Install dev tools into the container |
| `cella doctor` | Check system dependencies |
| `cella init` | Initialize a new devcontainer config |
| `cella code` | Open VS Code connected to the container |

## Next Steps

- [Configuration](configuration.md) -- settings reference, merge semantics, tool configuration
- [Worktrees](worktrees.md) -- parallel development, background tasks, in-container commands
- [Port Forwarding](port-forwarding.md) -- hostname routing, dev server setup, OrbStack
- [Network Proxy](network-proxy.md) -- traffic blocking, allowlists, CA certificates
- [Credential Protection](credential-protection.md) -- phantom tokens, threat model
