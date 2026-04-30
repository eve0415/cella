# Configuration

Cella uses a layered configuration system that lets you set defaults globally, override per-project, and embed settings in `devcontainer.json`. Config files use TOML (preferred) or JSON with JSONC comments.

## Config file locations

Cella loads configuration from three locations, merged in order:

| Priority | Path | Scope |
|----------|------|-------|
| 1 (lowest) | `~/.cella/config.toml` | Global defaults for all projects |
| 2 | `customizations.cella` in `devcontainer.json` | Shared with version control |
| 3 (highest) | `.devcontainer/cella.toml` | Project-level overrides |

Each location is optional. When no config files exist, all settings use their defaults.

The global and project-level configs can use either `.toml` or `.json` extension. When both exist in the same directory, the TOML file takes precedence and the JSON file is ignored. JSON files support JSONC comments (`//` and `/* */`).

### devcontainer.json

Settings in `devcontainer.json` go under `customizations.cella`:

```jsonc
{
  "image": "mcr.microsoft.com/devcontainers/base:ubuntu",
  "customizations": {
    "cella": {
      "credentials": { "gh": true },
      "tools": { "claude-code": { "version": "stable" } }
    }
  }
}
```

## Resolution order and merge semantics

Layers are merged from lowest to highest priority. The merge algorithm handles different value types differently:

| Value type | Behavior | Example |
|------------|----------|---------|
| Scalar (string, number, bool) | Higher-priority layer wins | `mode = "logged"` overrides `mode = "disabled"` |
| Object | Recursive merge — sibling fields are preserved | Setting `[credentials.ai] enabled = false` in project config doesn't remove `gh = true` from global |
| Array | Concatenated — all layers' entries are combined | `network.rules` from global + project are joined, not replaced |

Array concatenation is notable: if the global config defines two network rules and the project config defines one, the result has all three. This is intentional — it lets teams layer organizational rules with project-specific ones.

### Example

Global config (`~/.cella/config.toml`):

```toml
[credentials]
gh = true

[credentials.ai]
enabled = true
openai = false
```

Project config (`.devcontainer/cella.toml`):

```toml
[credentials.ai]
anthropic = false
```

Result after merge:

```toml
[credentials]
gh = true                # preserved from global

[credentials.ai]
enabled = true           # preserved from global
openai = false           # preserved from global
anthropic = false        # added by project
```

## Settings reference

All enum values are lowercase strings in config files.

### `[credentials]`

Controls credential forwarding into containers.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `gh` | bool | `true` | Forward GitHub CLI credentials |

```toml
[credentials]
gh = true
```

#### `[credentials.ai]`

Controls which AI provider API keys are forwarded from the host environment into containers. Unknown provider names default to enabled.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `true` | Global toggle — when `false`, no AI keys are forwarded |
| *`<provider>`* | bool | `true` | Per-provider override — known providers: `anthropic`, `openai`, `gemini`, `groq`, `mistral`, `deepseek`, `xai`, `fireworks`, `together`, `perplexity`, `cohere` |

```toml
[credentials.ai]
enabled = true
openai = false         # disable OpenAI key forwarding
anthropic = true       # explicitly enable (also the default)
```

When `enabled = false`, all per-provider toggles are ignored and no keys are forwarded.

### `[tools]`

Controls automatic installation and host config forwarding for dev tools. Each tool section can enable/disable the tool and control whether host configuration is bind-mounted into the container.

#### `[tools.claude-code]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `true` | Install Claude Code in the container |
| `forward_config` | bool | `true` | Bind-mount host config into the container |
| `version` | string | `"latest"` | `"latest"`, `"stable"`, or a pinned version like `"1.0.58"` |

Host paths mounted when `forward_config = true`:
- `~/.claude/` → `$HOME/.claude/`
- `~/.claude.json` → `$HOME/.claude.json`

```toml
[tools.claude-code]
enabled = true
forward_config = true
version = "latest"
```

#### `[tools.codex]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `true` | Install Codex CLI in the container |
| `forward_config` | bool | `true` | Bind-mount host config into the container |
| `version` | string | `"latest"` | `"latest"` or a pinned version like `"0.1.2"` |

Host paths mounted when `forward_config = true`:
- `~/.codex/` → `$HOME/.codex/`

```toml
[tools.codex]
enabled = true
version = "latest"
```

#### `[tools.gemini]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `true` | Install Gemini CLI in the container |
| `forward_config` | bool | `true` | Bind-mount host config into the container |
| `version` | string | `"latest"` | `"latest"` or a pinned version like `"0.1.2"` |

Host paths mounted when `forward_config = true`:
- `~/.gemini/` → `$HOME/.gemini/`

```toml
[tools.gemini]
enabled = true
version = "latest"
```

#### `[tools.nvim]`

Neovim does not have an `enabled` toggle — it is installed on-demand when first used. This section controls config forwarding and the install version.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `forward_config` | bool | `true` | Bind-mount host nvim config into the container |
| `version` | string | `"stable"` | `"stable"`, `"nightly"`, or a pinned version like `"0.10.3"` |
| `config_path` | string | *unset* | Override host config source directory (default: `~/.config/nvim`) |

Host paths mounted when `forward_config = true`:
- `~/.config/nvim/` (or custom `config_path`) → `$HOME/.config/nvim/`

The `config_path` option changes where cella reads config on the host — the container destination is always `$HOME/.config/nvim/`.

```toml
[tools.nvim]
forward_config = true
version = "stable"
config_path = "~/dotfiles/nvim"
```

#### `[tools.tmux]`

Tmux does not have an `enabled` toggle or a `version` field. This section controls config forwarding only.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `forward_config` | bool | `true` | Bind-mount host tmux config into the container |
| `config_path` | string | *unset* | Override host config source path (default: `~/.tmux.conf` or `~/.config/tmux/`) |

Host paths mounted when `forward_config = true`:
- `~/.tmux.conf` → `$HOME/.tmux.conf`
- `~/.config/tmux/` → `$HOME/.config/tmux/`

The `config_path` option changes where cella reads config on the host — the container destinations are always `$HOME/.tmux.conf` and `$HOME/.config/tmux/`.

```toml
[tools.tmux]
forward_config = true
config_path = "~/dotfiles/tmux"
```

### `[network]`

Network proxy and blocking settings. See [Network Proxy](network-proxy.md) for full details on modes, rules, path patterns, CA certificates, and CLI commands.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `mode` | string | *unset* | `"denylist"` or `"allowlist"` — when unset, does not override `devcontainer.json` |
| `proxy` | object | *(see below)* | Proxy configuration |
| `rules` | array | `[]` | Network blocking rules |

#### `[network.proxy]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `true` | Enable proxy forwarding |
| `http` | string | *unset* | HTTP proxy URL override |
| `https` | string | *unset* | HTTPS proxy URL override |
| `no_proxy` | string | *unset* | `NO_PROXY` override |
| `ca_cert` | string | *unset* | Path to additional CA certificate |
| `proxy_port` | integer | `18080` | In-container proxy listen port |

#### `[[network.rules]]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `domain` | string | *(required)* | Domain glob pattern |
| `paths` | array of strings | `[]` | Path glob patterns |
| `action` | string | *(required)* | `"block"` or `"allow"` |

### `[cli]`

Persisted defaults for CLI flags.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `skip_checksum` | bool | `false` | Skip agent binary checksum verification |
| `no_network_rules` | bool | `false` | Disable all network blocking rules |

```toml
[cli]
skip_checksum = false
no_network_rules = false
```

#### `[cli.build]`

Build-specific CLI defaults.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `no_cache` | bool | `false` | Disable Docker build cache |

```toml
[cli.build]
no_cache = false
```

## Validation

All config sections use strict validation — unknown fields are rejected. A typo like `[securityy]` or `enbled = true` produces an error at load time rather than being silently ignored.
