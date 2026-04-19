# cella-doctor

> System diagnostics and health checking for cella.

Part of the [cella](../../README.md) workspace.

## Overview

cella-doctor powers the `cella doctor` command. It runs structured health checks across six categories (system, Docker, git/credentials, daemon, configuration, containers) and can redact PII for safe sharing of diagnostic output. `hostRequirements` validation itself lives in `cella-orchestrator` (where the up pipeline needs it); cella-doctor surfaces the orchestrator's decision under the `config` category so `cella doctor` reports it alongside the other checks.

Each check category runs with a 5-second timeout to prevent hangs from unresponsive services. Results are collected into a structured report with severity levels (pass, warning, error, info) that can be output as human-readable text or JSON. When invoked inside a container, `cella doctor` reconciles the host view with the in-container view so the report reflects what the user actually sees (e.g. whether `BROWSER` interception is wired up on compose workspaces).

## Architecture

### Key Types

- `Severity` — check outcome level (Pass, Warning, Error, Info)
- `CheckResult` — single check result (name, severity, message, detail)
- `CategoryReport` — results for one check category
- `Report` — complete diagnostic report across all categories

### Modules

| Module | Purpose |
|--------|---------|
| `checks/` | Check orchestration, core types (`Severity`, `CheckResult`, `CategoryReport`, `Report`), timeout handling |
| `checks/system` | System checks — OS, architecture, available CPUs, memory, disk space |
| `checks/docker` | Docker connectivity, version, runtime detection (Docker Engine, OrbStack) |
| `checks/git` | Git availability, credential helper configuration, SSH agent status |
| `checks/daemon` | cella daemon health — running status, PID file, port file, socket connectivity |
| `checks/config` | Devcontainer configuration discovery and validation |
| `checks/container` | Running container inspection — state, mounts, labels, port bindings |
| `redact` | PII redaction — strips usernames, paths, tokens from diagnostic output for safe sharing |

## Crate Dependencies

**Depends on:** [cella-backend](../cella-backend), [cella-config](../cella-config), [cella-daemon](../cella-daemon), [cella-docker](../cella-docker), [cella-env](../cella-env), [cella-protocol](../cella-protocol)

**Depended on by:** [cella-cli](../cella-cli)

## Testing

```sh
cargo test -p cella-doctor
```

Unit tests cover host requirements parsing (memory size strings, CPU counts), PII redaction patterns, and check result assembly.

## Development

To add a new check category:
1. Create a new module in `checks/`
2. Implement a function that returns `Vec<CheckResult>`
3. Add the category to the orchestration in `checks/mod.rs`

The redaction module uses regex patterns to strip sensitive data. When adding new checks that might include PII (paths, usernames, tokens), ensure the redactor covers the new patterns.

Host requirements parsing must handle flexible formats — the spec allows "8gb", "8GB", "8192mb", etc. for memory, and the GPU field can be a boolean, string, or object. Follow the existing parsing patterns in `host_requirements.rs`.
