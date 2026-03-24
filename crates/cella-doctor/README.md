# cella-doctor

> System diagnostics, health checking, and hostRequirements validation.

Part of the [cella](../../README.md) workspace.

## Overview

cella-doctor powers the `cella doctor` command. It runs structured health checks across six categories (system, Docker, git/credentials, daemon, configuration, containers), validates host requirements from devcontainer.json, and can redact PII for safe sharing of diagnostic output.

Each check category runs with a 5-second timeout to prevent hangs from unresponsive services. Results are collected into a structured report with severity levels (pass, warning, error, info) that can be output as human-readable text or JSON.

The host requirements module validates `hostRequirements` from the devcontainer spec — checking available CPUs, memory, storage, and GPU against the requirements. By default unmet requirements produce warnings; `--strict host-requirements` makes them errors.

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
| `host_requirements` | `hostRequirements` spec validation — CPU, memory (parses "8gb"/"512mb"), storage, GPU detection |
| `redact` | PII redaction — strips usernames, paths, tokens from diagnostic output for safe sharing |

## Crate Dependencies

**Depends on:** [cella-config](../cella-config), [cella-daemon](../cella-daemon), [cella-docker](../cella-docker), [cella-env](../cella-env), [cella-port](../cella-port)

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
